use async_stream::stream;
use futures::StreamExt;

use crate::{
    AssistantMessage, InferenceError, InferenceRequest, ModelRegistry, ProviderRegistry,
    event_stream::{AssistantMessageEvent, FinishReason, InferenceStream},
    provider::ResolvedInferenceRequest,
};

#[derive(Default, Clone)]
pub struct InferenceClient {
    models: ModelRegistry,
    providers: ProviderRegistry,
}

impl InferenceClient {
    #[must_use]
    pub fn new(models: ModelRegistry, providers: ProviderRegistry) -> Self {
        Self { models, providers }
    }

    #[must_use]
    pub fn stream(&self, request: InferenceRequest) -> InferenceStream {
        let model = match self.models.get(&request.model_ref) {
            Ok(model) => model,
            Err(error) => return error_stream(error),
        };

        let provider = match self.providers.get(&model.api_family) {
            Ok(provider) => provider,
            Err(error) => return error_stream(error),
        };

        provider.stream(ResolvedInferenceRequest { model, request })
    }

    /// Runs an inference request to completion and returns the final assistant message.
    ///
    /// # Errors
    ///
    /// Returns an [`InferenceError`] when model/provider resolution fails, the provider emits an
    /// error event, or the provider stream ends without a final message.
    pub async fn complete(
        &self,
        request: InferenceRequest,
    ) -> Result<AssistantMessage, InferenceError> {
        let mut stream = self.stream(request);
        let mut latest: Option<AssistantMessage> = None;

        while let Some(event) = stream.next().await {
            match event {
                AssistantMessageEvent::Start { message }
                | AssistantMessageEvent::TextDelta { message, .. }
                | AssistantMessageEvent::ThinkingDelta { message, .. }
                | AssistantMessageEvent::ToolCallDelta { message, .. } => {
                    latest = Some(message);
                }
                AssistantMessageEvent::Done { message } => return Ok(message),
                AssistantMessageEvent::Error { error, .. } => return Err(error),
                AssistantMessageEvent::RawProviderEvent { .. } => {}
            }
        }

        latest.ok_or_else(|| InferenceError::ProviderProtocol {
            message: "provider stream ended without final message".to_owned(),
        })
    }
}

fn error_stream(error: InferenceError) -> InferenceStream {
    Box::pin(stream! {
        let message = AssistantMessage {
            content: Vec::new(),
            tool_calls: Vec::new(),
            finish_reason: Some(FinishReason::Error),
            usage: None,
        };

        yield AssistantMessageEvent::Error { error, message}
    })
}
