use std::sync::Arc;

use crate::{
    AssistantMessage,
    event::{AssistantMessageEvent, FinishReason},
    model::ApiFamily,
    registry::ProviderRegistry,
};

#[cfg(feature = "provider-faux")]
pub mod faux;

#[cfg(feature = "provider-openai-compat")]
pub mod openai_compat;

#[cfg(feature = "provider-openai-responses")]
pub mod openai_responses;

pub mod sse;

#[must_use]
pub fn error_event(error: crate::InferenceError) -> AssistantMessageEvent {
    AssistantMessageEvent::Error {
        error,
        message: AssistantMessage {
            content: Vec::new(),
            tool_calls: Vec::new(),
            finish_reason: Some(FinishReason::Error),
            usage: None,
        },
    }
}

/// Register only network-backed providers.
///
/// Keep this separate from the faux provider so tests can choose deterministic
/// behavior and production code can choose real model APIs.
pub fn register_openai_compat_provider(registry: &mut ProviderRegistry) {
    #[cfg(feature = "provider-openai-compat")]
    registry.register(
        ApiFamily::OpenAiChatCompletions,
        Arc::new(openai_compat::OpenAiCompatProvider::new()),
    );

    #[cfg(feature = "provider-openai-responses")]
    registry.register(
        ApiFamily::OpenAiResponses,
        Arc::new(openai_responses::OpenAiResponseProvider::new()),
    );
}

/// Register deterministic providers for tests and examples.
///
/// The faux provider should not be registered together with a real provider for
/// the same API family unless a test explicitly wants to override behavior.
#[cfg(feature = "provider-faux")]
pub fn register_test_provider(registry: &mut ProviderRegistry) {
    registry.register(
        ApiFamily::OpenAiChatCompletions,
        Arc::new(faux::FauxProvider::new()),
    );
}
