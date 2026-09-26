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

    /// reqwest's own text stops at "error sending request for url (...)";
    /// the part that says WHY (connection reset, DNS, TLS, timeout) sits in
    /// its `source()` chain, so the message spells that chain out.
    #[error("HTTP transport error: {}", with_causes(.0))]
    Http(#[from] reqwest::Error),

    /// The message stream closed or reported an error before producing a final message.
    #[error("message stream error: {0}")]
    Stream(#[from] MessageStreamError),

    #[error("{0}")]
    Other(String),
}

/// `err`, then each error in its `source()` chain, joined by `: `.
///
/// `successors` walks the linked list: start at the first cause, and keep
/// asking each cause for ITS cause until one answers `None`. The closure
/// gets `&&dyn Error`; the `&cause` pattern copies the inner reference out,
/// so the next cause borrows from the error itself, not from the closure's
/// short-lived argument (without it: "lifetime may not live long enough").
fn with_causes(err: &reqwest::Error) -> String {
    use core::error::Error as _;
    core::iter::successors(err.source(), |&cause| cause.source())
        .fold(err.to_string(), |text, cause| format!("{text}: {cause}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn transport_errors_name_their_cause() {
        // Bind a free port, then close it: connecting there is refused.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let err = reqwest::get(format!("http://{addr}")).await.unwrap_err();
        let text = InferenceError::from(err).to_string();
        // Without the chain this was only "... error sending request for url (...)".
        assert!(text.contains("Connection refused"), "{text}");
    }
}
