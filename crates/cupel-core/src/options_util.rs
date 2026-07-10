//! Request-option helpers shared by all providers: token estimation,
//! `max_tokens` clamping, and thinking-budget arithmetic.

use crate::types::{
    AssistantContent, Context, Message, StopReason, ThinkingBudgets, ThinkingLevel,
    ToolResultContent, Usage, UserContentBody,
};

/// Rough heuristic used across the industry: ~4 characters per token.
/// Exact tokenization is model-specific; for clamping purposes a cheap
/// estimate is enough (and pi uses the same constant).
const CHARS_PER_TOKEN: u64 = 4;
/// A base64 image is roughly this many "characters" worth of tokens.
const ESTIMATED_IMAGE_CHARS: u64 = 4800;
/// Head-room subtracted from the context window before clamping, so a
/// slightly-off estimate doesn't push the request over the limit.
const CONTEXT_SAFETY_TOKENS: u64 = 4096;
const MIN_MAX_TOKENS: u64 = 1;

fn estimate_text_tokens(text: &str) -> u64 {
    (text.len() as u64).div_ceil(CHARS_PER_TOKEN)
}

fn tokens_from_usage(usage: &Usage) -> u64 {
    if usage.total_tokens > 0 {
        usage.total_tokens
    } else {
        usage.input + usage.output + usage.cache_read + usage.cache_write
    }
}

fn estimate_message_tokens(message: &Message) -> u64 {
    let chars: u64 = match message {
        Message::User(user) => match &user.content {
            UserContentBody::Text(text) => text.len() as u64,
            UserContentBody::Blocks(blocks) => blocks
                .iter()
                .map(|b| match b {
                    crate::types::UserContent::Text(t) => t.text.len() as u64,
                    crate::types::UserContent::Image(_) => ESTIMATED_IMAGE_CHARS,
                })
                .sum(),
        },
        Message::ToolResult(result) => result
            .content
            .iter()
            .map(|b| match b {
                ToolResultContent::Text(t) => t.text.len() as u64,
                ToolResultContent::Image(_) => ESTIMATED_IMAGE_CHARS,
            })
            .sum(),
        Message::Assistant(assistant) => assistant
            .content
            .iter()
            .map(|b| match b {
                AssistantContent::Text(t) => t.text.len() as u64,
                AssistantContent::Thinking(t) => t.thinking.len() as u64,
                AssistantContent::ToolCall(tc) => {
                    tc.name.len() as u64 + tc.arguments.to_string().len() as u64
                }
            })
            .sum(),
    };
    chars.div_ceil(CHARS_PER_TOKEN)
}

/// Estimate the total tokens a context will occupy.
///
/// Trick from pi: the most recent successful assistant message carries
/// *exact* token usage from the provider. Use that as an anchor and only
/// estimate the messages that came after it - far more accurate than
/// estimating the entire transcript.
#[must_use]
pub fn estimate_context_tokens(context: &Context) -> u64 {
    let messages = &context.messages;

    let last_usage = messages.iter().enumerate().rev().find_map(|(i, m)| {
        let Message::Assistant(a) = m else {
            return None;
        };
        if matches!(a.stop_reason, StopReason::Error | StopReason::Aborted) {
            return None;
        }
        let tokens = tokens_from_usage(&a.usage);
        (tokens > 0).then_some((i, tokens))
    });

    if let Some((index, usage_tokens)) = last_usage {
        let trailing: u64 = messages[index + 1..]
            .iter()
            .map(estimate_message_tokens)
            .sum();
        return usage_tokens + trailing;
    }

    // No usage anchor: estimate everything, including the fixed prefix
    // (system prompt + tool schemas) that providers count as input.
    let mut tokens: u64 = messages.iter().map(estimate_message_tokens).sum();
    if let Some(system) = &context.system_prompt {
        tokens += estimate_text_tokens(system);
    }
    if let Some(tools) = &context.tools
        && !tools.is_empty()
        && let Ok(json) = serde_json::to_string(tools)
    {
        tokens += estimate_text_tokens(&json);
    }
    tokens
}

/// Clamp a requested `max_tokens` so `input + output` fits the model's
/// context window (with safety margin). Providers reject requests that
/// ask for more output than the remaining window.
#[must_use]
pub fn clamp_max_tokens_to_context(
    model: &crate::types::Model,
    context: &Context,
    max_tokens: u64,
) -> u64 {
    if model.context_window == 0 {
        return max_tokens.max(MIN_MAX_TOKENS);
    }
    let available = model
        .context_window
        .saturating_sub(estimate_context_tokens(context))
        .saturating_sub(CONTEXT_SAFETY_TOKENS);
    max_tokens.min(available.max(MIN_MAX_TOKENS))
}

/// Budget-based thinking models "spend" thinking tokens out of `max_tokens`,
/// so enabling thinking without raising `max_tokens` would starve the actual
/// answer. `xhigh` clamps to `high` (budget models don't support it).
#[must_use]
pub fn clamp_reasoning(level: ThinkingLevel) -> ThinkingLevel {
    match level {
        ThinkingLevel::XHigh => ThinkingLevel::High,
        other => other,
    }
}

/// Result of [`adjust_max_tokens_for_thinking`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdjustedThinking {
    pub max_tokens: u64,
    pub thinking_budget: u64,
}

/// Raise `max_tokens` to make room for the thinking budget (capped at the
/// model's limit), and shrink the budget when it wouldn't leave at least
/// 1024 tokens for the visible answer.
///
/// `base_max_tokens = None` means "the caller did not set an output cap":
/// use the model cap and fit thinking inside it.
#[must_use]
pub fn adjust_max_tokens_for_thinking(
    base_max_tokens: Option<u64>,
    model_max_tokens: u64,
    reasoning_level: ThinkingLevel,
    custom_budgets: Option<ThinkingBudgets>,
) -> AdjustedThinking {
    const MIN_OUTPUT_TOKENS: u64 = 1024;

    let budgets = custom_budgets.unwrap_or_default();
    let mut thinking_budget = match clamp_reasoning(reasoning_level) {
        ThinkingLevel::Minimal => budgets.minimal.unwrap_or(1024),
        ThinkingLevel::Low => budgets.low.unwrap_or(2048),
        ThinkingLevel::Medium => budgets.medium.unwrap_or(8192),
        ThinkingLevel::High | ThinkingLevel::XHigh => budgets.high.unwrap_or(16384),
    };

    let max_tokens = match base_max_tokens {
        None => model_max_tokens,
        Some(base) => (base + thinking_budget).min(model_max_tokens),
    };

    if max_tokens <= thinking_budget {
        thinking_budget = max_tokens.saturating_sub(MIN_OUTPUT_TOKENS);
    }

    AdjustedThinking {
        max_tokens,
        thinking_budget,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn budget_fits_inside_model_cap() {
        let adjusted =
            adjust_max_tokens_for_thinking(Some(4096), 8192, ThinkingLevel::Medium, None);
        // 4096 + 8192 budget would exceed the 8192 model cap -> capped.
        assert_eq!(adjusted.max_tokens, 8192);
        assert_eq!(adjusted.thinking_budget, 8192 - 1024);
    }

    #[test]
    fn xhigh_clamps_to_high_budget() {
        let adjusted = adjust_max_tokens_for_thinking(None, 64_000, ThinkingLevel::XHigh, None);
        assert_eq!(adjusted.thinking_budget, 16384);
        assert_eq!(adjusted.max_tokens, 64_000);
    }
}
