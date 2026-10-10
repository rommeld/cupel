//! Transient-error classification for failed assistant messages.
//!
//! When a turn ends with `stop_reason: Error`, the agent needs to decide:
//! is this worth retrying (a 529 "overloaded", a dropped connection), or
//! would a retry just burn money (invalid request, exhausted quota)? The
//! retained error category takes precedence over text heuristics. Provider
//! errors and older messages still use HTTP wrappers and provider wording;
//! quota/billing wording can veto retries even for a 429 response.
//!
//! This module only classifies. Retry policy (budget, backoff, restarting
//! the turn) lives in the agent loop.
//!
//! Implementation note: instead of pulling in the `regex` crate, we
//! *compress* both the message and the patterns: lowercase, keep
//! only `[a-z0-9]`. This makes "Rate Limit", "rate-limit", and "RateLimit" all
//! become "ratelimit". Same effect, one allocation, and the pattern tables
//! stay readable.

use crate::types::{AssistantMessage, ErrorKind, StopReason};

/// Hard request/account failures: retrying cannot help and may mask a real
/// problem from the user. Checked first because some of these arrive
/// wrapped in otherwise-retryable-looking 429 responses.
const NON_RETRYABLE_PATTERNS: &[&str] = &[
    // Safety refusals can carry a generic "Provider returned error" wrapper.
    "contentfilter",
    // Subscription/account limits (returned as 429s by some gateways).
    "gousagelimiterror",
    "freeusagelimiterror",
    "monthlyusagelimitreached",
    "availablebalance",
    // Quota/budget/billing exhaustion. "insufficientquota" is OpenAI's
    // billing error code; the rest cover common gateway wording.
    "insufficientquota",
    "outofbudget",
    "quotaexceeded",
    "billing",
];

/// Transient provider/transport failures worth retrying.
const RETRYABLE_PATTERNS: &[&str] = &[
    // Provider load and server-side transient failures.
    "overloaded",
    "ratelimit",
    "toomanyrequests",
    "throttling",
    "serviceunavailable",
    "servererror",
    "internalerror",
    "internalserverexception",
    "modelstreamerrorexception",
    // Wrapper/gateway text for transient upstream failures.
    "providerreturnederror",
    // Network / proxy / transport failures.
    "networkerror",
    "connectionerror",
    "connectionrefused",
    "connectionlost",
    "othersideclosed",
    "fetchfailed",
    "upstreamconnect",
    "resetbeforeheaders",
    "sockethangup",
    "timedout",
    "timeout",
    "terminated",
    // The same failures in reqwest's words: "error sending request" means
    // nothing came back. "error decoding response body" means the stream
    // broke mid-response.
    "errorsendingrequest",
    "errordecodingresponsebody",
    // WebSocket transports report close/error text instead of HTTP text.
    "websocketclosed",
    "websocketerror",
    // Premature stream endings (our own providers emit the middle one).
    "endedwithout",
    "streamendedbeforemessagestop",
    "http2requestdidnotgetaresponse",
    // Provider-requested retry-delay failures should flow through the
    // outer retry policy.
    "retrydelay",
    // Explicit retry guidance emitted mid-stream by OpenAI and Bedrock.
    "youcanretryyourrequest",
    "tryyourrequestagain",
    "pleaseretryyourrequest",
];

/// Lowercase and strip everything but letters and digits, so word
/// separators never defeat a match. Shared with [`crate::overflow`].
pub(crate) fn compress(text: &str) -> String {
    text.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// 408 (request timeout, e.g. OpenRouter) and 429 are worth another attempt,
/// as is any 5xx. Every other status describes the request itself.
fn retryable_status(status: u16) -> bool {
    matches!(status, 408 | 429) || (500..600).contains(&status)
}

/// Recognize only explicit status wrappers, never numbers inside arbitrary bodies.
/// Shared with [`crate::overflow`] so both classifiers read the same status.
pub(crate) fn legacy_http_status(text: &str) -> Option<u16> {
    let status = if let Some(rest) = text.strip_prefix("Error Code ") {
        rest.split_once(':')?.0.trim().parse().ok()?
    } else {
        let rest = text
            .strip_prefix("provider returned HTTP ")
            .or_else(|| text.strip_prefix("HTTP "))?;
        rest.split(|c: char| c == ':' || c.is_ascii_whitespace())
            .next()?
            .parse()
            .ok()?
    };
    (100..600).contains(&status).then_some(status)
}

/// Does this failed assistant message look like a transient provider or
/// transport error, i.e. should the caller consider restarting the turn?
///
/// Only `stop_reason: Error` qualifies because an `Aborted` message means the
/// user cancelled, and retrying against the user's intent would be hostile.
#[must_use]
pub fn is_retryable_assistant_error(message: &AssistantMessage) -> bool {
    if message.stop_reason != StopReason::Error {
        return false;
    }
    let error_message = message.error_message.as_deref().unwrap_or_default();
    let status = match message.error_kind {
        Some(ErrorKind::Aborted | ErrorKind::Config) => return false,
        Some(ErrorKind::Transport) => return true,
        Some(ErrorKind::HttpStatus { status }) => Some(status),
        Some(ErrorKind::Provider) | None => legacy_http_status(error_message),
    };
    if status.is_some_and(|status| !retryable_status(status)) {
        return false;
    }
    let compressed = compress(error_message);
    if NON_RETRYABLE_PATTERNS
        .iter()
        .any(|pattern| compressed.contains(pattern))
    {
        return false;
    }
    status.is_some()
        || RETRYABLE_PATTERNS
            .iter()
            .any(|pattern| compressed.contains(pattern))
}

/// Preserved thinking: Anthropic binds every thinking block to the
/// conversation that produced it and answers a replayed block with a 400
/// once that conversation changed ("... The block is bound to a different
/// conversation. ..."). A tampered signature fails with the same first
/// sentence but not this one, and stripping would not be the fix there.
const THINKING_BINDING_PATTERN: &str = "boundtoadifferentconversation";

/// Did the provider reject a replayed thinking block because the history
/// changed since the block was produced? Unlike the transient errors above,
/// resending the same request fails the same way; it succeeds once the
/// stale thinking blocks are gone (the agent loop strips them).
#[must_use]
pub fn is_thinking_binding_mismatch(message: &AssistantMessage) -> bool {
    message.stop_reason == StopReason::Error
        && matches!(
            message.error_kind,
            None | Some(ErrorKind::Provider | ErrorKind::HttpStatus { status: 400 })
        )
        && message
            .error_message
            .as_deref()
            .is_some_and(|error| compress(error).contains(THINKING_BINDING_PATTERN))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Api, Provider, Usage};

    fn message(stop_reason: StopReason, error: Option<&str>) -> AssistantMessage {
        AssistantMessage {
            content: Vec::new(),
            api: Api::from(Api::ANTHROPIC_MESSAGES),
            provider: Provider::from(Provider::ANTHROPIC),
            model: "m".into(),
            response_model: None,
            response_id: None,
            usage: Usage::default(),
            stop_reason,
            error_message: error.map(str::to_string),
            error_kind: None,
            timestamp: 0,
        }
    }

    #[test]
    fn transient_errors_are_retryable() {
        for error in [
            "Overloaded",
            "provider returned HTTP 529: overloaded_error",
            "Rate limit exceeded, try again later",
            "rate-limited",
            "HTTP 503 Service Unavailable",
            "Internal server error",
            "connection refused",
            "Anthropic stream ended before message_stop",
            "Request timed out",
            "Throttling error: Too many requests, please retry your request",
            // reqwest transport failures, before and during the response.
            "HTTP transport error: error sending request for url (https://chatgpt.com/backend-api/codex/responses)",
            "HTTP transport error: error decoding response body",
        ] {
            assert!(
                is_retryable_assistant_error(&message(StopReason::Error, Some(error))),
                "expected retryable: {error}"
            );
        }
    }

    #[test]
    fn quota_and_billing_errors_are_not_retryable() {
        for error in [
            "insufficient_quota: check your plan and billing details",
            "Monthly usage limit reached",
            "quota exceeded for this billing period",
            // A 429 wrapper around a hard account limit must not retry.
            "429: FreeUsageLimitError",
        ] {
            for kind in [None, Some(ErrorKind::HttpStatus { status: 429 })] {
                let mut message = message(StopReason::Error, Some(error));
                message.error_kind = kind;
                assert!(
                    !is_retryable_assistant_error(&message),
                    "expected non-retryable: {error} ({kind:?})"
                );
            }
        }
    }

    #[test]
    fn categories_override_misleading_or_missing_text() {
        for (kind, expected) in [
            (ErrorKind::Aborted, false),
            (ErrorKind::Config, false),
            (ErrorKind::Transport, true),
            (ErrorKind::HttpStatus { status: 400 }, false),
            (ErrorKind::HttpStatus { status: 401 }, false),
            (ErrorKind::HttpStatus { status: 403 }, false),
            (ErrorKind::HttpStatus { status: 408 }, true),
            (ErrorKind::HttpStatus { status: 429 }, true),
            (ErrorKind::HttpStatus { status: 500 }, true),
            (ErrorKind::HttpStatus { status: 502 }, true),
            (ErrorKind::HttpStatus { status: 503 }, true),
            (ErrorKind::HttpStatus { status: 504 }, true),
            (ErrorKind::HttpStatus { status: 529 }, true),
        ] {
            for text in [None, Some("overloaded Error Code 400: request id 500429")] {
                let mut message = message(StopReason::Error, text);
                message.error_kind = Some(kind);
                assert_eq!(
                    is_retryable_assistant_error(&message),
                    expected,
                    "{kind:?}: {text:?}"
                );
            }
        }
    }

    #[test]
    fn digits_in_error_bodies_are_not_retry_indicators() {
        for code in [429, 500, 502, 503, 504] {
            for text in [
                format!("invalid request: identifier {code}"),
                format!("invalid request: requested {code}0 tokens"),
                format!("provider returned HTTP 400: request {code}, upstream overloaded"),
            ] {
                for kind in [
                    None,
                    Some(ErrorKind::Provider),
                    Some(ErrorKind::HttpStatus { status: 400 }),
                ] {
                    let mut message = message(StopReason::Error, Some(&text));
                    message.error_kind = kind;
                    assert!(!is_retryable_assistant_error(&message), "{kind:?}: {text}");
                }
            }
        }
    }

    #[test]
    fn explicit_stream_client_errors_override_transient_wrapper_text() {
        for code in [400, 401, 402, 403, 404, 409, 422] {
            let error = format!("Error Code {code}: Provider returned error");
            assert!(
                !is_retryable_assistant_error(&message(StopReason::Error, Some(&error))),
                "expected non-retryable: {error}"
            );
        }
        for code in [408, 429, 500, 502, 503, 504] {
            let error = format!("Error Code {code}: Provider returned error");
            assert!(is_retryable_assistant_error(&message(
                StopReason::Error,
                Some(&error)
            )));
        }
        assert!(!is_retryable_assistant_error(&message(
            StopReason::Error,
            Some("Error Code content_filter: Provider returned error")
        )));
    }

    #[test]
    fn non_error_stop_reasons_never_retry() {
        // Aborted = the user cancelled; retrying would override their intent.
        assert!(!is_retryable_assistant_error(&message(
            StopReason::Aborted,
            Some("overloaded")
        )));
        assert!(!is_retryable_assistant_error(&message(
            StopReason::Stop,
            None
        )));
    }

    #[test]
    fn unrecognized_errors_are_not_retryable() {
        assert!(!is_retryable_assistant_error(&message(
            StopReason::Error,
            Some("invalid_request: max_tokens must be positive")
        )));
    }

    #[test]
    fn thinking_binding_mismatch_is_recognized() {
        // Anthropic's wording for a block replayed after a history edit.
        let stale = "provider returned HTTP 400: {\"type\":\"error\",\"error\":{\"type\":\"invalid_request_error\",\"message\":\"messages.5.content.0: Invalid `signature` in `thinking` block. The block is bound to a different conversation. Remove the block, or set `thinking.block_binding.prefix_mismatch_behavior` to \\\"drop_block\\\".\"}}";
        assert!(is_thinking_binding_mismatch(&message(
            StopReason::Error,
            Some(stale)
        )));
        // A tampered signature has only the first sentence: not ours.
        let tampered = "provider returned HTTP 400: messages.5.content.0: Invalid `signature` in `thinking` block";
        assert!(!is_thinking_binding_mismatch(&message(
            StopReason::Error,
            Some(tampered)
        )));
        // Cancelled by the user: never recovered behind their back.
        assert!(!is_thinking_binding_mismatch(&message(
            StopReason::Aborted,
            Some(stale)
        )));
    }
}
