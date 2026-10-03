//! Protocol adapters ("providers").
//!
//! Each submodule translates between the unified types in [`crate::types`]
//! and one vendor wire protocol:
//!
//! - [`anthropic`]: Anthropic Messages API (SSE)
//! - [`openai_responses`]: `OpenAI` Responses API (SSE)
//! - [`openai_completions`]: `OpenAI` Chat Completions API (SSE), the
//!   protocol most "OpenAI-compatible" vendors (Fireworks, OpenRouter, ...) speak
//! - [`openai_codex_responses`]: the ChatGPT Codex backend's Responses
//!   dialect (SSE), driven by ChatGPT OAuth tokens instead of API keys
//! - [`bedrock`]: AWS Bedrock `ConverseStream` (binary event stream via the
//!   official AWS SDK)
//!
//! All providers follow the same stream functions:
//! `stream()` returns immediately; the network work happens on a spawned
//! Tokio task; *every* failure after that point is delivered as an `Error`
//! event on the stream, never as a panic.

pub mod anthropic;
pub mod bedrock;
pub mod openai_codex_responses;
pub mod openai_completions;
pub mod openai_responses;

use crate::event_stream::EventSink;
use crate::types::{AssistantMessage, Model, StopReason, StreamOptions, Usage, now_ms};

pub(crate) const CONTENT_FILTER_MESSAGE: &str =
    "The provider's content filter stopped the response. Partial output may be incomplete.";

/// Build the skeleton assistant message a provider accumulates into while
/// streaming. Every provider starts from this same shape.
#[must_use]
pub(crate) fn new_output_message(model: &Model) -> AssistantMessage {
    AssistantMessage {
        content: Vec::new(),
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        response_model: None,
        response_id: None,
        usage: Usage::default(),
        stop_reason: StopReason::Stop,
        error_message: None,
        timestamp: now_ms(),
    }
}

/// Build the minimal error message emitted when a provider task fails before
/// (or instead of) producing a terminal event.
#[must_use]
pub(crate) fn error_message(model: &Model, reason: StopReason, text: String) -> AssistantMessage {
    AssistantMessage {
        stop_reason: reason,
        error_message: Some(text),
        ..new_output_message(model)
    }
}

/// Finish a protocol-complete response without discarding its content or usage.
/// Mapped error stops are terminal Error events, not worker failures that the
/// outer wrapper would replace with an empty error message.
pub(crate) fn finish_output(mut output: AssistantMessage, sink: &EventSink) {
    let reason = output.stop_reason;
    if matches!(reason, StopReason::Error | StopReason::Aborted) {
        output
            .error_message
            .get_or_insert_with(|| "The provider stopped generation with an error".to_string());
    }
    log_completion(&output);
    let _ = match reason {
        StopReason::Error | StopReason::Aborted => sink.error(reason, output),
        _ => sink.done(reason, output),
    };
}

/// Await a future, racing it against the caller's cancellation token.
///
/// This is the Rust analogue of passing an `AbortSignal` into fetch: instead
/// of the signal being threaded through the HTTP client, we `select!` between
/// "the work" and "cancellation" at every await point that can block.
pub(crate) async fn with_cancel<T>(
    options: &StreamOptions,
    fut: impl core::future::Future<Output = T>,
) -> Result<T, crate::error::InferenceError> {
    match &options.signal {
        Some(token) => {
            tokio::select! {
                // `biased` checks cancellation first on every poll, so a
                // cancelled request never completes "by accident".
                biased;
                () = token.cancelled() => Err(crate::error::InferenceError::Aborted),
                value = fut => Ok(value),
            }
        }
        None => Ok(fut.await),
    }
}

/// Log the terminal outcome of one provider request. This is the
/// observability record for cost accounting: one INFO line per request with
/// exact token counts and dollars. Request duration comes from the enclosing
/// provider span (emitted on span close when the subscriber enables span
/// events), so it isn't duplicated here.
pub(crate) fn log_completion(message: &AssistantMessage) {
    tracing::info!(
        stop_reason = ?message.stop_reason,
        input_tokens = message.usage.input,
        output_tokens = message.usage.output,
        cache_read_tokens = message.usage.cache_read,
        cache_write_tokens = message.usage.cache_write,
        cost_usd = message.usage.cost.total,
        response_id = message.response_id.as_deref().unwrap_or(""),
        "provider request complete"
    );
}

/// Apply model-level then option-level custom headers to a request builder
/// (option-level wins).
pub(crate) fn apply_custom_headers(
    mut req: reqwest::RequestBuilder,
    model: &Model,
    options: &StreamOptions,
) -> reqwest::RequestBuilder {
    if let Some(headers) = &model.headers {
        for (key, value) in headers {
            req = req.header(key.as_str(), value.as_str());
        }
    }
    if let Some(headers) = &options.headers {
        for (key, value) in headers {
            req = req.header(key.as_str(), value.as_str());
        }
    }
    req
}

#[cfg(test)]
mod tests {
    use crate::error::MessageStreamError;
    use crate::event_stream::assistant_message_channel;
    use crate::providers::{finish_output, new_output_message};
    use crate::types::{AssistantContent, StopReason, TextContent};

    #[tokio::test]
    async fn terminal_stops_preserve_the_accumulated_message() {
        let model = crate::catalog::builtin_models().into_iter().next().unwrap();
        for reason in [
            StopReason::Stop,
            StopReason::Length,
            StopReason::ToolUse,
            StopReason::Error,
            StopReason::Aborted,
        ] {
            let mut output = new_output_message(&model);
            output.stop_reason = reason;
            output.content = vec![AssistantContent::Text(TextContent::plain("partial output"))];
            output.response_id = Some("response-id".into());
            output.response_model = Some("served-model".into());
            output.usage.input = 100;
            output.usage.output = 12;
            output.usage.cache_read = 30;
            output.usage.total_tokens = 142;
            output.usage.cost.total = 0.25;
            let failed = matches!(reason, StopReason::Error | StopReason::Aborted);
            if failed {
                output.error_message = Some("provider stop explanation".into());
            }

            let (stream, sink) = assistant_message_channel();
            finish_output(output.clone(), &sink);
            drop(sink);
            let actual = match stream.result().await {
                Ok(message) => {
                    assert!(!failed, "error stops must not be emitted as Done");
                    message
                }
                Err(MessageStreamError::ProviderError {
                    reason: reported,
                    message,
                }) => {
                    assert!(failed);
                    assert_eq!(reported, reason);
                    *message
                }
                Err(error) => panic!("unexpected stream error: {error}"),
            };
            assert_eq!(actual, output);
        }
    }
}
