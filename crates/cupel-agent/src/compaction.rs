//! Context compaction: keep long sessions inside the model's context window
//! by replacing old history with an LLM-generated summary.
//!
//! Port of pi's `harness/compaction/compaction.ts`, simplified where
//! documented. The moving parts:
//!
//! - **Estimation** anchors on the last successful assistant message's
//!   provider-reported usage (exact) and only estimates what came after -
//!   far more accurate than estimating the whole transcript.
//! - **The threshold**: compaction fires when estimated tokens exceed
//!   `context_window - reserve_tokens`. The reserve leaves room for the
//!   summarization prompt AND the next turn's output.
//! - **The cut point** walks back from the end accumulating
//!   ~`keep_recent_tokens` of recent messages, then cuts at a user-message
//!   boundary. Cutting elsewhere could separate a tool call from its result;
//!   user messages always start a fresh turn. (pi additionally summarizes a
//!   split turn's prefix separately - deferred; our fallback cuts at the
//!   nearest user boundary even if that keeps a little more than the
//!   budget.)
//! - **The summary** is one non-streaming LLM call using pi's structured
//!   checkpoint format (Goal / Progress / Decisions / Next Steps). When a
//!   previous summary exists (second-or-later compaction), it is UPDATED
//!   rather than regenerated, so long sessions don't lose early context.
//! - The summary re-enters the transcript as a plain user message with a
//!   marker header. pi uses a dedicated `compactionSummary` role that
//!   converts to a user message at the LLM boundary; ours IS that user
//!   message (the marker makes it recognizable for iterative updates).

use std::sync::Arc;

use tokio_util::sync::CancellationToken;

use cupel_core::{
    provider::Registry,
    types::{
        AssistantContent, Context, Message, Model, StreamOptions, ToolResultContent,
        UserContentBody,
    },
};

use crate::types::{AgentContext, AgentMessage};

/// Marker prefixed to the summary user message. Doubles as the way to find
/// the previous summary for iterative updates.
pub const COMPACTION_MARKER: &str = "[Conversation summary - earlier history was compacted]";

/// Thresholds and retention settings (pi's defaults).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactionConfig {
    pub enabled: bool,
    /// Tokens reserved for the summarization prompt and the next output.
    pub reserve_tokens: u64,
    /// Approximate recent-context tokens kept verbatim after compaction.
    pub keep_recent_tokens: u64,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            reserve_tokens: 16_384,
            keep_recent_tokens: 20_000,
        }
    }
}

// ---------------------------------------------------------------------------
// Estimation
// ---------------------------------------------------------------------------

const CHARS_PER_TOKEN: u64 = 4;
const ESTIMATED_IMAGE_CHARS: u64 = 4800;

fn estimate_message_tokens(message: &AgentMessage) -> u64 {
    let chars: u64 = match message {
        AgentMessage::Custom { payload, .. } => payload.to_string().len() as u64,
        AgentMessage::Llm(Message::User(user)) => match &user.content {
            UserContentBody::Text(text) => text.len() as u64,
            UserContentBody::Blocks(blocks) => blocks
                .iter()
                .map(|b| match b {
                    cupel_core::types::UserContent::Text(t) => t.text.len() as u64,
                    cupel_core::types::UserContent::Image(_) => ESTIMATED_IMAGE_CHARS,
                })
                .sum(),
        },
        AgentMessage::Llm(Message::Assistant(assistant)) => assistant
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
        AgentMessage::Llm(Message::ToolResult(result)) => result
            .content
            .iter()
            .map(|b| match b {
                ToolResultContent::Text(t) => t.text.len() as u64,
                ToolResultContent::Image(_) => ESTIMATED_IMAGE_CHARS,
            })
            .sum(),
    };
    chars.div_ceil(CHARS_PER_TOKEN)
}

/// Estimate total context tokens for the next request: last exact provider
/// usage + estimates for everything after it (+ fixed prefix when no usage
/// anchor exists yet).
#[must_use]
pub fn estimate_context_tokens(context: &AgentContext) -> u64 {
    let messages = &context.messages;

    let anchor = messages.iter().enumerate().rev().find_map(|(i, m)| {
        let AgentMessage::Llm(Message::Assistant(a)) = m else {
            return None;
        };
        if matches!(
            a.stop_reason,
            cupel_core::types::StopReason::Error | cupel_core::types::StopReason::Aborted
        ) {
            return None;
        }
        let total = if a.usage.total_tokens > 0 {
            a.usage.total_tokens
        } else {
            a.usage.input + a.usage.output + a.usage.cache_read + a.usage.cache_write
        };
        (total > 0).then_some((i, total))
    });

    if let Some((index, usage_tokens)) = anchor {
        let trailing: u64 = messages[index + 1..]
            .iter()
            .map(estimate_message_tokens)
            .sum();
        return usage_tokens + trailing;
    }

    // No anchor: estimate everything including system prompt + tool schemas.
    let mut tokens: u64 = messages.iter().map(estimate_message_tokens).sum();
    tokens += (context.system_prompt.len() as u64).div_ceil(CHARS_PER_TOKEN);
    for tool in &context.tools {
        tokens += ((tool.description().len() + tool.parameters().to_string().len()) as u64)
            .div_ceil(CHARS_PER_TOKEN);
    }
    tokens
}

/// Should this context be compacted before the next request?
#[must_use]
pub fn should_compact(context_tokens: u64, context_window: u64, config: &CompactionConfig) -> bool {
    if !config.enabled || context_window == 0 {
        return false;
    }
    context_tokens > context_window.saturating_sub(config.reserve_tokens)
}

// ---------------------------------------------------------------------------
// Cut-point selection
// ---------------------------------------------------------------------------

/// Index of the first message KEPT verbatim. Everything before it gets
/// summarized. Walks back accumulating the keep budget, then snaps to the
/// next user/custom message boundary (never between a tool call and its
/// result).
fn find_cut_index(messages: &[AgentMessage], keep_recent_tokens: u64) -> usize {
    let mut accumulated: u64 = 0;
    let mut budget_start = messages.len();
    for (i, message) in messages.iter().enumerate().rev() {
        accumulated += estimate_message_tokens(message);
        budget_start = i;
        if accumulated >= keep_recent_tokens {
            break;
        }
    }
    // Snap forward to a turn boundary at or after the budget start.
    for (i, message) in messages.iter().enumerate().skip(budget_start) {
        if matches!(
            message,
            AgentMessage::Llm(Message::User(_)) | AgentMessage::Custom { .. }
        ) {
            return i;
        }
    }
    // No boundary in the tail (one giant turn): keep only the tail from the
    // budget start. transform_messages synthesizes tool results for any
    // orphaned calls, so even this cut is wire-safe.
    budget_start
}

// ---------------------------------------------------------------------------
// Summarization
// ---------------------------------------------------------------------------

// pi's prompts, verbatim - the structured format is what makes summaries
// actionable for the model that continues the work.
pub const SUMMARIZATION_SYSTEM_PROMPT: &str = "You are a context summarization assistant. Your task is to read a conversation between a user and an AI assistant, then produce a structured summary following the exact format specified.\n\nDo NOT continue the conversation. Do NOT respond to any questions in the conversation. ONLY output the structured summary.";

const SUMMARIZATION_PROMPT: &str = "The messages above are a conversation to summarize. Create a structured context checkpoint summary that another LLM will use to continue the work.\n\nUse this EXACT format:\n\n## Goal\n[What is the user trying to accomplish? Can be multiple items if the session covers different tasks.]\n\n## Constraints & Preferences\n- [Any constraints, preferences, or requirements mentioned by user]\n- [Or \"(none)\" if none were mentioned]\n\n## Progress\n### Done\n- [x] [Completed tasks/changes]\n\n### In Progress\n- [ ] [Current work]\n\n### Blocked\n- [Issues preventing progress, if any]\n\n## Key Decisions\n- **[Decision]**: [Brief rationale]\n\n## Next Steps\n1. [Ordered list of what should happen next]\n\n## Critical Context\n- [Any data, examples, or references needed to continue]\n- [Or \"(none)\" if not applicable]\n\nKeep each section concise. Preserve exact file paths, function names, and error messages.";

const UPDATE_SUMMARIZATION_PROMPT: &str = "The messages above are NEW conversation messages to incorporate into the existing summary provided in <previous-summary> tags.\n\nUpdate the existing structured summary with new information. RULES:\n- PRESERVE all existing information from the previous summary\n- ADD new progress, decisions, and context from the new messages\n- UPDATE the Progress section: move items from \"In Progress\" to \"Done\" when completed\n- UPDATE \"Next Steps\" based on what was accomplished\n- PRESERVE exact file paths, function names, and error messages\n- If something is no longer relevant, you may remove it\n\nUse the same EXACT format as the previous summary (Goal / Constraints & Preferences / Progress / Key Decisions / Next Steps / Critical Context).\n\nKeep each section concise. Preserve exact file paths, function names, and error messages.";

/// Cap for tool results inside the serialized conversation - full outputs
/// would blow the summarization request itself.
const SERIALIZED_TOOL_RESULT_CHARS: usize = 2000;

/// Flatten messages to readable text for the summarization prompt.
fn serialize_conversation(messages: &[AgentMessage]) -> String {
    let mut out = String::new();
    for message in messages {
        match message {
            AgentMessage::Llm(Message::User(user)) => {
                out.push_str("[User]\n");
                match &user.content {
                    UserContentBody::Text(text) => out.push_str(text),
                    UserContentBody::Blocks(blocks) => {
                        for block in blocks {
                            match block {
                                cupel_core::types::UserContent::Text(t) => out.push_str(&t.text),
                                cupel_core::types::UserContent::Image(_) => {
                                    out.push_str("(image)");
                                }
                            }
                            out.push('\n');
                        }
                    }
                }
            }
            AgentMessage::Llm(Message::Assistant(assistant)) => {
                out.push_str("[Assistant]\n");
                for block in &assistant.content {
                    match block {
                        AssistantContent::Text(t) => out.push_str(&t.text),
                        // Thinking is the model's scratch space, not durable
                        // context - skip it like pi's convertToLlm does.
                        AssistantContent::Thinking(_) => continue,
                        AssistantContent::ToolCall(tc) => {
                            out.push_str(&format!("[tool call: {} {}]", tc.name, tc.arguments));
                        }
                    }
                    out.push('\n');
                }
            }
            AgentMessage::Llm(Message::ToolResult(result)) => {
                out.push_str(&format!("[Tool result: {}]\n", result.tool_name));
                for block in &result.content {
                    if let ToolResultContent::Text(t) = block {
                        let text: String =
                            t.text.chars().take(SERIALIZED_TOOL_RESULT_CHARS).collect();
                        out.push_str(&text);
                        if t.text.len() > text.len() {
                            out.push_str("\n[...truncated]");
                        }
                        out.push('\n');
                    }
                }
            }
            AgentMessage::Custom { kind, payload, .. } => {
                out.push_str(&format!("[{kind}]\n{payload}\n"));
            }
        }
        out.push('\n');
    }
    out
}

/// Why compaction failed. The loop treats failures as non-fatal (the next
/// request may still fit; if not, the overflow error reaches the user).
#[derive(Debug, thiserror::Error)]
pub enum CompactionError {
    #[error("nothing to compact")]
    NothingToCompact,
    #[error("summarization failed: {0}")]
    SummarizationFailed(String),
}

/// Outcome of a successful compaction, for events/telemetry.
#[derive(Debug, Clone)]
pub struct CompactionOutcome {
    pub tokens_before: u64,
    pub tokens_after: u64,
    pub summarized_messages: usize,
}

/// Compact `context.messages` in place: summarize everything before the cut
/// point via one LLM call, then splice `[summary user message] + kept tail`.
pub async fn compact(
    context: &mut AgentContext,
    registry: &Arc<Registry>,
    model: &Model,
    api_key: Option<String>,
    config: &CompactionConfig,
    cancel: &CancellationToken,
) -> Result<CompactionOutcome, CompactionError> {
    let tokens_before = estimate_context_tokens(context);
    let cut = find_cut_index(&context.messages, config.keep_recent_tokens);
    if cut == 0 {
        return Err(CompactionError::NothingToCompact);
    }
    let (to_summarize, kept) = context.messages.split_at(cut);

    // Iterative update: if a previous compaction summary is in the section
    // being summarized, feed it to the update prompt instead of losing it.
    let previous_summary = to_summarize.iter().find_map(|m| match m {
        AgentMessage::Llm(Message::User(user)) => match &user.content {
            UserContentBody::Text(text) if text.starts_with(COMPACTION_MARKER) => {
                Some(text[COMPACTION_MARKER.len()..].trim().to_string())
            }
            _ => None,
        },
        _ => None,
    });

    // ---- The summarization request -----------------------------------------
    let conversation = serialize_conversation(to_summarize);
    let mut prompt = format!("<conversation>\n{conversation}\n</conversation>\n\n");
    if let Some(previous) = &previous_summary {
        prompt.push_str(&format!(
            "<previous-summary>\n{previous}\n</previous-summary>\n\n"
        ));
    }
    prompt.push_str(if previous_summary.is_some() {
        UPDATE_SUMMARIZATION_PROMPT
    } else {
        SUMMARIZATION_PROMPT
    });

    // The summary must fit in the reserve (that's what it's reserved FOR).
    let max_tokens = (config.reserve_tokens * 8 / 10).min(model.max_tokens.max(1));
    let summarization_context = Context {
        system_prompt: Some(SUMMARIZATION_SYSTEM_PROMPT.to_string()),
        messages: vec![Message::User(cupel_core::types::UserMessage {
            content: UserContentBody::Text(prompt),
            timestamp: cupel_core::types::now_ms(),
        })],
        tools: None,
    };
    let options = StreamOptions {
        api_key,
        max_tokens: Some(max_tokens),
        signal: Some(cancel.clone()),
        ..StreamOptions::default()
    };

    let response = registry
        .complete(model, summarization_context, options)
        .await
        .map_err(|e| CompactionError::SummarizationFailed(e.to_string()))?;
    let summary: String = response
        .content
        .iter()
        .filter_map(|block| match block {
            AssistantContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    if summary.trim().is_empty() {
        return Err(CompactionError::SummarizationFailed(
            "summarization produced no text".to_string(),
        ));
    }

    // ---- Splice the transcript ------------------------------------------------
    let summarized_messages = to_summarize.len();
    let mut new_messages = vec![AgentMessage::user_text(format!(
        "{COMPACTION_MARKER}\n\n{summary}"
    ))];
    new_messages.extend(kept.iter().cloned());
    context.messages = new_messages;

    Ok(CompactionOutcome {
        tokens_before,
        tokens_after: estimate_context_tokens(context),
        summarized_messages,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(text: &str) -> AgentMessage {
        AgentMessage::user_text(text)
    }

    #[test]
    fn threshold_respects_reserve() {
        let config = CompactionConfig::default();
        // Window 100k, reserve ~16k: 80k is fine, 90k must compact.
        assert!(!should_compact(80_000, 100_000, &config));
        assert!(should_compact(90_000, 100_000, &config));
        // Disabled never compacts.
        let disabled = CompactionConfig {
            enabled: false,
            ..config
        };
        assert!(!should_compact(90_000, 100_000, &disabled));
    }

    #[test]
    fn cut_index_lands_on_a_user_boundary() {
        // ~1000 tokens each (4000 chars); keep budget of 1500 tokens should
        // keep roughly the last two messages, cutting at the user boundary.
        let big = "x".repeat(4000);
        let messages = vec![user(&big), user(&big), user(&big), user(&big)];
        let cut = find_cut_index(&messages, 1500);
        assert_eq!(cut, 2);
    }

    #[test]
    fn cut_index_zero_when_everything_fits_budget() {
        let messages = vec![user("short"), user("also short")];
        assert_eq!(find_cut_index(&messages, 20_000), 0);
    }

    #[test]
    fn serialization_truncates_tool_results() {
        let long_result =
            AgentMessage::Llm(Message::ToolResult(cupel_core::types::ToolResultMessage {
                tool_call_id: "c".into(),
                tool_name: "bash".into(),
                content: vec![ToolResultContent::Text(
                    cupel_core::types::TextContent::plain("y".repeat(10_000)),
                )],
                details: None,
                is_error: false,
                timestamp: 0,
            }));
        let text = serialize_conversation(&[long_result]);
        assert!(text.contains("[...truncated]"));
        assert!(text.len() < 5_000);
    }
}
