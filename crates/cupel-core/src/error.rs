//! Error types for the crate.
//!
//! Implementation to deliver an `InferenceError` instead of a `panic` after start to stream.

use thiserror::Error;

use crate::types::{AssistantMessage, ErrorKind, Provider, StopReason};

pub type Result<T> = core::result::Result<T, InferenceError>;

/// Error produced while collecting the final message from an
/// [`AssistantMessageStream`](crate::event_stream::AssistantMessageStream).
#[derive(Debug, Clone, Error)]
pub enum MessageStreamError {
    /// Provider emitted an `Error` event. The text carries the provider's own
    /// message, so a caller that only keeps this error (compaction's
    /// summarization call) still says what went wrong.
    #[error(
        "provider reported error: {}",
        .message.error_message.as_deref().unwrap_or("no details")
    )]
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
    #[error("no API key for provider: {0}{hint}", hint = missing_api_key_hint(.0))]
    MissingApiKey(String),

    /// The upstream HTTP API returned a non-2xx status. We keep the body
    /// so the caller can see the provider's error JSON.
    #[error("provider returned HTTP {status}: {body}")]
    ApiStatus { status: u16, body: String },

    /// The request was cancelled via the abort signal.
    #[error("request was aborted")]
    Aborted,

    /// reqwest's own text stops at "error sending request for url (...)";
    /// the part that says why (connection reset, DNS, TLS, timeout) sits in
    /// its `source()` chain, so the message spells that chain out.
    #[error("HTTP transport error: {}", with_causes(.0))]
    Http(#[from] reqwest::Error),

    /// The message stream closed or reported an error before producing a final message.
    #[error("message stream error: {0}")]
    Stream(#[from] MessageStreamError),

    /// The connection failed or broke before the response was complete,
    /// outside reqwest (the AWS SDK) or as a stream that ended without its
    /// terminal event. Sending the request again may succeed.
    #[error("{0}")]
    Transport(String),

    /// The request cannot be built from the configuration (credentials,
    /// region, endpoint). Sending it again cannot help.
    #[error("{0}")]
    Config(String),

    #[error("{0}")]
    Other(String),
}

impl InferenceError {
    /// Preserve actionable information before the display text crosses a stream boundary.
    #[must_use]
    pub fn kind(&self) -> ErrorKind {
        match self {
            Self::NoProvider(_) | Self::MissingApiKey(_) | Self::Config(_) => ErrorKind::Config,
            Self::ApiStatus { status, .. } => ErrorKind::HttpStatus { status: *status },
            Self::Aborted => ErrorKind::Aborted,
            Self::Http(error) if error.is_builder() => ErrorKind::Config,
            Self::Http(_) | Self::Transport(_) => ErrorKind::Transport,
            Self::Stream(MessageStreamError::ProviderError { reason, message }) => message
                .error_kind
                .unwrap_or(if *reason == StopReason::Aborted {
                    ErrorKind::Aborted
                } else {
                    ErrorKind::Provider
                }),
            // Every provider ends its stream with a terminal event, even on
            // failure, so a channel that closes without one means the provider
            // task died (a panic). Resending the same request repeats the bug.
            Self::Stream(MessageStreamError::ClosedBeforeTerminalEvent) | Self::Other(_) => {
                ErrorKind::Provider
            }
        }
    }
}

fn missing_api_key_hint(provider: &str) -> &'static str {
    if provider == Provider::OPENAI_CODEX {
        " - no usable ChatGPT login: retry if the network was down, \
         otherwise run /login openai-codex in the TUI"
    } else {
        ""
    }
}

/// `err`, then each error in its `source()` chain, joined by `: `.
///
/// `successors` walks the linked list: start at the first cause, and keep
/// asking each cause for its cause until one answers `None`. The closure
/// gets `&&dyn Error`; the `&cause` pattern copies the inner reference out,
/// so the next cause borrows from the error itself, not from the closure's
/// short-lived argument (without it: "lifetime may not live long enough").
pub(crate) fn with_causes(err: &dyn core::error::Error) -> String {
    core::iter::successors(err.source(), |&cause| cause.source())
        .fold(err.to_string(), |text, cause| format!("{text}: {cause}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_codex_credentials_point_to_login_without_changing_the_category() {
        let error = InferenceError::MissingApiKey(Provider::OPENAI_CODEX.to_string());
        assert!(error.to_string().contains("/login openai-codex"));
        assert_eq!(error.kind(), ErrorKind::Config);

        assert_eq!(
            InferenceError::MissingApiKey(Provider::ANTHROPIC.to_string()).to_string(),
            "no API key for provider: anthropic"
        );
    }

    #[tokio::test]
    async fn transport_errors_name_their_cause() {
        // Bind a free port, then close it: connecting there is refused.
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let err = reqwest::get(format!("http://{addr}")).await.unwrap_err();
        let error = InferenceError::from(err);
        assert_eq!(error.kind(), ErrorKind::Transport);
        let text = error.to_string();
        // Without the chain this was only "... error sending request for url (...)".
        assert!(text.contains("Connection refused"), "{text}");
    }

    #[test]
    fn inference_variants_retain_their_categories() {
        for (error, expected) in [
            (InferenceError::NoProvider("api".into()), ErrorKind::Config),
            (
                InferenceError::MissingApiKey("provider".into()),
                ErrorKind::Config,
            ),
            (
                InferenceError::ApiStatus {
                    status: 429,
                    body: "opaque".into(),
                },
                ErrorKind::HttpStatus { status: 429 },
            ),
            (InferenceError::Aborted, ErrorKind::Aborted),
            (InferenceError::Other("opaque".into()), ErrorKind::Provider),
            (
                InferenceError::Transport("stream ended early".into()),
                ErrorKind::Transport,
            ),
            (
                InferenceError::Config("no region".into()),
                ErrorKind::Config,
            ),
            (
                InferenceError::Stream(MessageStreamError::ClosedBeforeTerminalEvent),
                ErrorKind::Provider,
            ),
        ] {
            assert_eq!(error.kind(), expected);
        }
        let error = reqwest::Client::new()
            .get("://invalid-url")
            .build()
            .unwrap_err();
        assert_eq!(InferenceError::from(error).kind(), ErrorKind::Config);
    }

    #[test]
    fn collected_provider_errors_say_what_went_wrong() {
        let model = crate::catalog::builtin_models().remove(0);
        let failed = crate::providers::error_message(
            &model,
            &InferenceError::Other("model is overloaded".into()),
        );
        let collected = InferenceError::Stream(MessageStreamError::ProviderError {
            reason: failed.stop_reason,
            message: Box::new(failed),
        });
        assert_eq!(
            collected.to_string(),
            "message stream error: provider reported error: model is overloaded"
        );

        // A provider task that died without a terminal event is a bug, so
        // resending the same request would only repeat it.
        let closed = crate::providers::error_message(
            &model,
            &InferenceError::Stream(MessageStreamError::ClosedBeforeTerminalEvent),
        );
        assert!(!crate::retry::is_retryable_assistant_error(&closed));
    }

    #[test]
    fn collecting_a_failed_message_keeps_the_original_category() {
        let model = crate::catalog::builtin_models().remove(0);
        let error = InferenceError::ApiStatus {
            status: 403,
            body: "denied".into(),
        };
        let message = crate::providers::error_message(&model, &error);
        let collected = InferenceError::Stream(MessageStreamError::ProviderError {
            reason: message.stop_reason,
            message: Box::new(message),
        });
        assert_eq!(collected.kind(), error.kind());
    }
}
