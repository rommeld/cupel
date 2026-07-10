//! Protocol adapters ("providers").
//!
//! Each submodule translates between the unified types in [`crate::types`]
//! and one vendor wire protocol:
//!
//! - [`anthropic`] - Anthropic Messages API (SSE)
//! - [`openai_responses`] - `OpenAI` Responses API (SSE)
//! - [`openai_completions`] - `OpenAI` Chat Completions API (SSE) - the
//!   protocol most "OpenAI-compatible" vendors (Fireworks, Groq, ...) speak
//! - [`bedrock`] - AWS Bedrock `ConverseStream` (binary event stream via the
//!   official AWS SDK)
//!
//! All providers follow the same stream functions:
//! `stream()` returns immediately; the network work happens on a spawned
//! Tokio task; *every* failure after that point is delivered as an `Error`
//! event on the stream, never as a panic.

pub mod anthropic;
pub mod bedrock;
pub mod openai_completions;
pub mod openai_responses;

use crate::types::{AssistantMessage, Model, StopReason, StreamOptions, Usage, now_ms};

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

/// Log the terminal outcome of one provider request. This is THE
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
/// (option-level wins, matching pi's merge order).
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
