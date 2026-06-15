//! "Fake" provider for deterministic tests.
//!
//! - Streaming collection
//! - Context transformation
//! - Tool-Call handling
//! - Runtime loop behavior
//! - CLI display
use async_stream::stream;
use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use crate::{
    context::{AssistantMessage, ContentBlock},
    event_stream::{AssistantMessageEvent, FinishReason, InferenceStream},
    provider::{InferenceProvider, ResolvedInferenceRequest},
};
#[derive(Default, Clone)]
pub struct FauxProvider {
    responses: Arc<Mutex<VecDeque<String>>>,
}

impl FauxProvider {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Queues a text response for the next faux inference stream.
    ///
    /// # Panics
    ///
    /// Panics if the internal response queue mutex is poisoned.
    pub fn push_text_response<T>(&self, text: T)
    where
        T: Into<String>,
    {
        self.responses
            .lock()
            .expect("faux provider lock poisoned.")
            .push_back(text.into());
    }
}

impl InferenceProvider for FauxProvider {
    fn stream(&self, _request: ResolvedInferenceRequest) -> InferenceStream {
        let response = self
            .responses
            .lock()
            .expect("faux provider lock poisoned.")
            .pop_front()
            .unwrap_or_else(|| "faux response".to_owned());

        Box::pin(stream! {
            let mut message = AssistantMessage {
                content: Vec::new(),
                tool_calls: Vec::new(),
                finish_reason: None,
                usage: None,
            };

            yield AssistantMessageEvent::Start {
                message: message.clone()
            };

            for chunk in response.split_whitespace() {
                let delta = format!("{chunk} ");
                message.content.push(ContentBlock::Text {
                    text: delta.clone(),
                });

                yield AssistantMessageEvent::TextDelta {
                    delta,
                    message: message.clone(),
                };
            }
            message.finish_reason = Some(FinishReason::Stop);

            yield AssistantMessageEvent::Done { message }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        api_registry::{ModelRegistry, ProviderRegistry},
        client::InferenceClient,
        context::InferenceContext,
        event_stream::FinishReason,
        model::{
            ApiFamily, ContextWindow, ModelId, ModelRef, ModelSpec, ProviderId, ReasoningSupport,
        },
        provider::{InferenceRequest, InferenceRequestOptions},
    };
    use std::sync::Arc;

    #[tokio::test]
    async fn faux_provider_can_complete() {
        let faux = FauxProvider::new();
        faux.push_text_response("hello cupel");

        let mut models = ModelRegistry::new();
        models.insert(ModelSpec {
            model_ref: ModelRef("test/faux".to_owned()),
            provider: ProviderId("faux".to_owned()),
            api_family: ApiFamily::OpenAiChatCompletions,
            model_id: ModelId("faux-model".to_owned()),
            base_url: None,
            display_name: Some("Faux".to_owned()),
            context_window: ContextWindow {
                input_tokens: 128_000,
                output_tokens: None,
            },
            reasoning: ReasoningSupport::None,
            pricing: None,
        });

        let mut providers = ProviderRegistry::new();
        providers.register(crate::ApiFamily::OpenAiChatCompletions, Arc::new(faux));

        let client = InferenceClient::new(models, providers);

        let message = client
            .complete(InferenceRequest {
                model_ref: ModelRef("test/faux".to_owned()),
                context: InferenceContext::default(),
                tools: Vec::new(),
                options: InferenceRequestOptions::default(),
            })
            .await
            .expect("faux completion should succeed");

        assert_eq!(message.finish_reason, Some(FinishReason::Stop));
    }
}
