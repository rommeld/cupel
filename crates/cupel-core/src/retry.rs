//! Transient-error classification for failed assistant messages.
//!
//! When a turn ends with `stop_reason: Error`, the agent needs to decide:
//! is this worth retrying (a 529 "overloaded", a dropped connection), or
//! would a retry just burn money (invalid request, exhausted quota)? The
//! answer lives in the error TEXT because that's all providers give us -
//! our unified error path collapses HTTP status, SDK exception names, and
//! stream-level error events into one message string.
//!
//! This module only CLASSIFIES. Retry policy (budget, backoff, restarting
//! the turn) lives in the agent loop - same split as pi, where this
//! classifier sits in the ai package and the policy in agent-session.
//!
//! Implementation note: pi matches with regexes like `rate.?limit` (any
//! separator between the words). Instead of pulling in the `regex` crate for
//! that, we *compress* both the message and the patterns - lowercase, keep
//! only `[a-z0-9]` - so "Rate Limit", "rate-limit", and "RateLimit" all
//! become "ratelimit". Same effect, one allocation, and the pattern tables
//! stay readable.

use crate::types::{AssistantMessage, StopReason};

/// Account/billing limits: retrying cannot help and may mask a real
/// problem from the user. Checked FIRST because some of these arrive
/// wrapped in otherwise-retryable-looking 429 responses.
const NON_RETRYABLE_PATTERNS: &[&str] = &[
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
    "429",
    "500",
    "502",
    "503",
    "504",
    "serviceunavailable",
    "servererror",
    "internalerror",
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
/// separators never defeat a match.
fn compress(text: &str) -> String {
    text.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// Does this failed assistant message look like a TRANSIENT provider or
/// transport error, i.e. should the caller consider restarting the turn?
///
/// Only `stop_reason: Error` qualifies - an `Aborted` message means the
/// user cancelled, and retrying against the user's intent would be hostile.
#[must_use]
pub fn is_retryable_assistant_error(message: &AssistantMessage) -> bool {
    if message.stop_reason != StopReason::Error {
        return false;
    }
    let Some(error_message) = &message.error_message else {
        return false;
    };
    let compressed = compress(error_message);
    if NON_RETRYABLE_PATTERNS
        .iter()
        .any(|pattern| compressed.contains(pattern))
    {
        return false;
    }
    RETRYABLE_PATTERNS
        .iter()
        .any(|pattern| compressed.contains(pattern))
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
            // A 429 wrapper around a hard account limit must NOT retry.
            "429: FreeUsageLimitError",
        ] {
            assert!(
                !is_retryable_assistant_error(&message(StopReason::Error, Some(error))),
                "expected non-retryable: {error}"
            );
        }
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
}
