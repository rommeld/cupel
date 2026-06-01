use std::sync::Arc;

use crate::{model::ApiFamily, registry::ProviderRegistry};

pub mod anthropic;
pub mod faux;
pub mod openai_compat;
pub mod openai_responses;

#[cfg(feature = "provider-openai-compat")]
pub fn register_openai_compat_provider(registry: &mut ProviderRegistry) {
    registry.register(
        ApiFamily::OpenAiChatCompletions,
        Arc::new(openai_compat::OpenAiCompatProvider::new()),
    );
}

#[cfg(feature = "provider-faux")]
pub fn register_test_provider(registry: &mut ProviderRegistry) {
    registry.register(
        ApiFamily::OpenAiChatCompletions,
        Arc::new(faux::FauxProvider::new()),
    );
}
