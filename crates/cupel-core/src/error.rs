//! Error types for the crate.
//!
//! Implementation to deliver an `InferenceError` instead of a `panic` after start to stream.

use thiserror::Error;

use crate::types::{AssistantMessage, StopReason};

pub type Result<T> = core::result::Result<T, InferenceError>;

/// Error produced while collecting the final message from an
/// [`AssistantMessageStream`](crate::event_stream::AssistantMessageStream).
#[derive(Debug, Clone, Error)]
pub enum MessageStreamError {
    /// Provider emitted an `Error` event.
    #[error("provider reported error: {reason:?}")]
    ProviderError {
        reason: StopReason,
        message: Box<AssistantMessage>,
    },
    /// The channel closed before any `Done` or `Error` terminal event.
    #[error("stream closed before terminal event")]
    ClosedBeforeTerminalEvent,
}

#[derive(Debug, Error)]
pub enum InferenceError {
    /// No provider was registered for a model's `api`.
    #[error("no API provider registered for api: {0}")]
    NoProvider(String),

    /// A provider needed an API key but none was supplied.
    #[error("no API key for provider: {0}")]
    MissingApiKey(String),

    /// The upstream HTTP API returned a non-2xx status. We keep the body
    /// so the caller can see the provider's error JSON.
    #[error("provider returned HTTP {status}: {body}")]
    ApiStatus { status: u16, body: String },

    /// The request was cancelled via the abort signal.
    #[error("request was aborted")]
    Aborted,

    #[error("HTTP transport error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("JSON error: {0}")]
    JsonError(#[from] serde_json::Error),

    /// The message stream closed or reported an error before producing a final message.
    #[error("message stream error: {0}")]
    Stream(#[from] MessageStreamError),

    #[error("{0}")]
    Other(String),
}
