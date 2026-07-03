//! Context-overflow detection. Port of pi's `utils/overflow.ts`.
//!
//! When the transcript outgrows the model's context window, providers fail
//! in three different ways - and the caller must recognize all of them to
//! trigger compaction instead of surfacing a dead-end error:
//!
//! 1. **Error-based** (most providers): `stop_reason: Error` with a
//!    provider-specific message ("prompt is too long", "exceeds the context
//!    window", ...). Matched against a pattern table below.
//! 2. **Silent acceptance** (z.ai style): the request "succeeds" but
//!    `usage.input` exceeds the context window.
//! 3. **Silent truncation** (Xiaomi MiMo style): input is truncated to fill
//!    the window exactly, leaving no room to generate - detected as
//!    `stop_reason: Length` with zero output and a full window.
//!
//! Pattern matching uses the same compression trick as [`crate::retry`]
//! (lowercase, alphanumerics only) instead of regexes. pi's patterns with
//! `.*` gaps become multi-part patterns: every part must appear in the
//! message.

use crate::types::{AssistantMessage, StopReason};

/// Overflow indicators. Multi-part entries require ALL parts present
/// (pi expresses these as regexes with `.*` gaps). Comments name the
/// provider whose wording each entry matches.
const OVERFLOW_PATTERNS: &[&[&str]] = &[
    &["promptistoolong"],                              // Anthropic (token overflow)
    &["requesttoolarge"],                              // Anthropic (byte overflow, HTTP 413)
    &["inputistoolongforrequestedmodel"],              // Amazon Bedrock
    &["exceedsthecontextwindow"],                      // OpenAI (Completions & Responses)
    &["exceeds", "maximumcontextlength"],              // OpenAI-compatible proxies (LiteLLM)
    &["inputtokencount", "exceedsthemaximum"],         // Google (Gemini)
    &["maximumpromptlengthis"],                        // xAI (Grok)
    &["reducethelengthofthemessages"],                 // Groq
    &["maximumcontextlengthis", "tokens"],             // OpenRouter (most backends)
    &["exceeds", "maximumallowedinputlength"],         // OpenRouter/Poolside
    &["islongerthanthemodelscontextlength"],           // Together AI
    &["exceedsthelimitof"],                            // GitHub Copilot
    &["exceedstheavailablecontextsize"],               // llama.cpp server
    &["greaterthanthecontextlength"],                  // LM Studio
    &["contextwindowexceedslimit"],                    // MiniMax
    &["exceededmodeltokenlimit"],                      // Kimi For Coding
    &["toolargeformodelwith", "maximumcontextlength"], // Mistral
    &["prompthas", "tokensbuttheconfiguredcontextsizeis"], // DS4 server
    &["modelcontextwindowexceeded"],                   // Bedrock/z.ai finish reason as text
    &["prompttoolongexceeded", "contextlength"],       // Ollama explicit overflow
    &["contextlengthexceeded"],                        // generic fallback
    &["toomanytokens"],                                // generic fallback
    &["tokenlimitexceeded"],                           // generic fallback
];

/// Messages matching these are NOT overflow even when an overflow pattern
/// also matches. Example: Bedrock throttling says "Too many tokens, please
/// wait before trying again" - that's rate limiting, not overflow.
const NON_OVERFLOW_PATTERNS: &[&str] = &[
    "throttlingerror",
    "serviceunavailable",
    "ratelimit",
    "toomanyrequests",
];

fn compress(text: &str) -> String {
    text.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// Does this assistant message indicate the request overflowed the model's
/// context window? `context_window` (tokens) enables the silent-overflow
/// checks; pass 0 to skip them.
#[must_use]
pub fn is_context_overflow(message: &AssistantMessage, context_window: u64) -> bool {
    // Case 1: explicit overflow error.
    if message.stop_reason == StopReason::Error
        && let Some(error_message) = &message.error_message
    {
        let compressed = compress(error_message);
        let is_non_overflow = NON_OVERFLOW_PATTERNS
            .iter()
            .any(|pattern| compressed.contains(pattern));
        if !is_non_overflow
            && OVERFLOW_PATTERNS
                .iter()
                .any(|parts| parts.iter().all(|part| compressed.contains(part)))
        {
            return true;
        }
    }

    if context_window == 0 {
        return false;
    }
    let input_tokens = message.usage.input + message.usage.cache_read;

    // Case 2: silent acceptance - "success" with more input than fits.
    if message.stop_reason == StopReason::Stop && input_tokens > context_window {
        return true;
    }

    // Case 3: silent truncation - the window is full and generation got
    // zero tokens of room.
    if message.stop_reason == StopReason::Length
        && message.usage.output == 0
        && input_tokens as f64 >= context_window as f64 * 0.99
    {
        return true;
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Api, Provider, Usage};

    fn message(stop_reason: StopReason, error: Option<&str>, usage: Usage) -> AssistantMessage {
        AssistantMessage {
            content: Vec::new(),
            api: Api::from(Api::ANTHROPIC_MESSAGES),
            provider: Provider::from(Provider::ANTHROPIC),
            model: "m".into(),
            response_model: None,
            response_id: None,
            usage,
            stop_reason,
            error_message: error.map(str::to_string),
            timestamp: 0,
        }
    }

    #[test]
    fn provider_overflow_errors_are_detected() {
        for error in [
            "prompt is too long: 213462 tokens > 200000 maximum",
            "413 {\"error\":{\"type\":\"request_too_large\"}}",
            "Input is too long for requested model.",
            "Your input exceeds the context window of this model",
            "Requested token count exceeds the model's maximum context length of 131072 tokens",
            "The input token count (1196265) exceeds the maximum number of tokens allowed",
            "This model's maximum prompt length is 131072",
            "Please reduce the length of the messages or completion",
            "This endpoint's maximum context length is 8192 tokens",
            "context_length_exceeded",
        ] {
            assert!(
                is_context_overflow(
                    &message(StopReason::Error, Some(error), Usage::default()),
                    200_000
                ),
                "expected overflow: {error}"
            );
        }
    }

    #[test]
    fn throttling_that_mentions_tokens_is_not_overflow() {
        // Bedrock: matches "too many tokens" but the prefix excludes it.
        assert!(!is_context_overflow(
            &message(
                StopReason::Error,
                Some("Throttling error: Too many tokens, please wait before trying again."),
                Usage::default(),
            ),
            200_000
        ));
    }

    #[test]
    fn silent_overflow_is_detected_via_usage() {
        let usage = Usage {
            input: 190_000,
            cache_read: 20_000,
            ..Usage::default()
        };
        assert!(is_context_overflow(
            &message(StopReason::Stop, None, usage),
            200_000
        ));
    }

    #[test]
    fn silent_truncation_is_detected_via_full_window_and_zero_output() {
        let usage = Usage {
            input: 199_500,
            output: 0,
            ..Usage::default()
        };
        assert!(is_context_overflow(
            &message(StopReason::Length, None, usage),
            200_000
        ));
        // Same shape but output was generated: an ordinary length stop.
        let usage = Usage {
            input: 199_500,
            output: 400,
            ..Usage::default()
        };
        assert!(!is_context_overflow(
            &message(StopReason::Length, None, usage),
            200_000
        ));
    }

    #[test]
    fn ordinary_errors_are_not_overflow() {
        assert!(!is_context_overflow(
            &message(StopReason::Error, Some("overloaded"), Usage::default()),
            200_000
        ));
    }
}
