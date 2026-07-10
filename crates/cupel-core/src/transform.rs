//! Cross-provider message normalization, applied by every provider before it
//! converts the unified transcript to its wire format.
//!
//! A transcript can contain messages produced by a *different* model than the
//! one about to be called (the user switched models mid-session, or a tool
//! result came from elsewhere). Providers are strict about what they accept
//! back, so this pass:
//!
//! 1. Replaces images with a text placeholder for models without vision.
//! 2. Converts foreign thinking blocks to plain text (signatures are only
//!    valid for the exact model that produced them) and drops redacted
//!    thinking that a different model cannot decrypt.
//! 3. Normalizes tool-call ids to the target provider's format (Anthropic
//!    requires `^[a-zA-Z0-9_-]{1,64}$`; `OpenAI` Responses ids can be 450+
//!    chars with `|`).
//! 4. Skips errored/aborted assistant messages entirely (incomplete turns
//!    that would be rejected on replay).
//! 5. Inserts synthetic error tool-results for tool calls that never got an
//!    answer, because every provider requires call/result pairing.

use crate::types::{
    AssistantContent, AssistantMessage, InputModality, Message, Model, StopReason, TextContent,
    ToolCall, ToolResultContent, ToolResultMessage, UserContent, UserContentBody, now_ms,
};
use std::collections::{HashMap, HashSet};

const NON_VISION_USER_IMAGE_PLACEHOLDER: &str = "(image omitted: model does not support images)";
const NON_VISION_TOOL_IMAGE_PLACEHOLDER: &str =
    "(tool image omitted: model does not support images)";

/// How a provider rewrites tool-call ids for cross-model replay.
///
/// A function pointer (not a closure trait object) keeps the signature simple;
/// none of the normalizers need captured state beyond the arguments.
pub type NormalizeToolCallId = fn(&str, &Model, &AssistantMessage) -> String;

/// See module docs. Providers call this first, then convert to wire format.
#[must_use]
pub fn transform_messages(
    messages: &[Message],
    model: &Model,
    normalize_tool_call_id: Option<NormalizeToolCallId>,
) -> Vec<Message> {
    // Old id -> normalized id, so tool *results* can follow their calls.
    let mut tool_call_id_map: HashMap<String, String> = HashMap::new();

    // ---- Pass 1: per-message rewrites -----------------------------------
    let transformed: Vec<Message> = messages
        .iter()
        .map(|msg| match msg {
            Message::User(user) => Message::User(downgrade_user_images(user.clone(), model)),
            Message::ToolResult(result) => {
                let mut result = downgrade_tool_result_images(result.clone(), model);
                if let Some(normalized) = tool_call_id_map.get(&result.tool_call_id) {
                    result.tool_call_id = normalized.clone();
                }
                Message::ToolResult(result)
            }
            Message::Assistant(assistant) => Message::Assistant(transform_assistant(
                assistant,
                model,
                normalize_tool_call_id,
                &mut tool_call_id_map,
            )),
        })
        .collect();

    // ---- Pass 2: drop broken turns, pair up orphaned tool calls ----------
    let mut result: Vec<Message> = Vec::with_capacity(transformed.len());
    let mut pending_tool_calls: Vec<ToolCall> = Vec::new();
    let mut existing_result_ids: HashSet<String> = HashSet::new();

    // Local helper as a closure won't work here (it would borrow `result`
    // mutably while we also push to it), so use a fn-style helper.
    fn insert_synthetic_results(
        result: &mut Vec<Message>,
        pending: &mut Vec<ToolCall>,
        existing: &mut HashSet<String>,
    ) {
        for tc in pending.drain(..) {
            if !existing.contains(&tc.id) {
                result.push(Message::ToolResult(ToolResultMessage {
                    tool_call_id: tc.id,
                    tool_name: tc.name,
                    content: vec![ToolResultContent::Text(TextContent::plain(
                        "No result provided",
                    ))],
                    details: None,
                    is_error: true,
                    timestamp: now_ms(),
                }));
            }
        }
        existing.clear();
    }

    for msg in transformed {
        match msg {
            Message::Assistant(assistant) => {
                // A new assistant turn: resolve any dangling calls first.
                insert_synthetic_results(
                    &mut result,
                    &mut pending_tool_calls,
                    &mut existing_result_ids,
                );

                // Errored/aborted turns are incomplete; replaying them causes
                // API errors (e.g. OpenAI's "reasoning without following item").
                if matches!(
                    assistant.stop_reason,
                    StopReason::Error | StopReason::Aborted
                ) {
                    continue;
                }

                pending_tool_calls = assistant
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        AssistantContent::ToolCall(tc) => Some(tc.clone()),
                        _ => None,
                    })
                    .collect();
                result.push(Message::Assistant(assistant));
            }
            Message::ToolResult(tool_result) => {
                existing_result_ids.insert(tool_result.tool_call_id.clone());
                result.push(Message::ToolResult(tool_result));
            }
            Message::User(user) => {
                // A user message interrupts the tool flow; close it out.
                insert_synthetic_results(
                    &mut result,
                    &mut pending_tool_calls,
                    &mut existing_result_ids,
                );
                result.push(Message::User(user));
            }
        }
    }

    // The transcript may end with unanswered tool calls.
    insert_synthetic_results(
        &mut result,
        &mut pending_tool_calls,
        &mut existing_result_ids,
    );

    result
}

fn model_supports_images(model: &Model) -> bool {
    model.input.contains(&InputModality::Image)
}

fn downgrade_user_images(
    mut user: crate::types::UserMessage,
    model: &Model,
) -> crate::types::UserMessage {
    if model_supports_images(model) {
        return user;
    }
    if let UserContentBody::Blocks(blocks) = &user.content {
        let replaced = replace_images(blocks.iter().map(|b| match b {
            UserContent::Text(t) => ContentRef::Text(t),
            UserContent::Image(_) => ContentRef::Image,
        }));
        user.content =
            UserContentBody::Blocks(replaced.into_iter().map(UserContent::Text).collect());
    }
    user
}

fn downgrade_tool_result_images(mut result: ToolResultMessage, model: &Model) -> ToolResultMessage {
    if model_supports_images(model) {
        return result;
    }
    let replaced = replace_images_with(
        result.content.iter().map(|b| match b {
            ToolResultContent::Text(t) => ContentRef::Text(t),
            ToolResultContent::Image(_) => ContentRef::Image,
        }),
        NON_VISION_TOOL_IMAGE_PLACEHOLDER,
    );
    result.content = replaced.into_iter().map(ToolResultContent::Text).collect();
    result
}

/// Borrowed view over "text or image" blocks so one replacement routine can
/// serve both user content and tool-result content. The image payload is
/// irrelevant here (it gets replaced), so the variant carries no data.
enum ContentRef<'a> {
    Text(&'a TextContent),
    Image,
}

fn replace_images<'a>(blocks: impl Iterator<Item = ContentRef<'a>>) -> Vec<TextContent> {
    replace_images_with(blocks, NON_VISION_USER_IMAGE_PLACEHOLDER)
}

/// Swap every image for a placeholder, collapsing *runs* of images into a
/// single placeholder so ten screenshots don't become ten identical lines.
fn replace_images_with<'a>(
    blocks: impl Iterator<Item = ContentRef<'a>>,
    placeholder: &str,
) -> Vec<TextContent> {
    let mut out: Vec<TextContent> = Vec::new();
    let mut previous_was_placeholder = false;
    for block in blocks {
        match block {
            ContentRef::Image => {
                if !previous_was_placeholder {
                    out.push(TextContent::plain(placeholder));
                }
                previous_was_placeholder = true;
            }
            ContentRef::Text(text) => {
                previous_was_placeholder = text.text == placeholder;
                out.push(text.clone());
            }
        }
    }
    out
}

fn transform_assistant(
    assistant: &AssistantMessage,
    model: &Model,
    normalize_tool_call_id: Option<NormalizeToolCallId>,
    tool_call_id_map: &mut HashMap<String, String>,
) -> AssistantMessage {
    // "Same model" means id AND provider AND api all match; only then are
    // opaque signatures (thinking, tool-call pairing) valid for replay.
    let is_same_model = assistant.provider == model.provider
        && assistant.api == model.api
        && assistant.model == model.id;

    let content: Vec<AssistantContent> = assistant
        .content
        .iter()
        .filter_map(|block| match block {
            AssistantContent::Thinking(thinking) => {
                // Redacted thinking is encrypted content only the producing
                // model can decrypt; drop it cross-model.
                if thinking.redacted == Some(true) {
                    return is_same_model.then(|| block.clone());
                }
                // Same model keeps signed thinking even with empty text
                // (OpenAI stores encrypted reasoning in the signature).
                if is_same_model && thinking.thinking_signature.is_some() {
                    return Some(block.clone());
                }
                if thinking.thinking.trim().is_empty() {
                    return None;
                }
                if is_same_model {
                    Some(block.clone())
                } else {
                    // Cross-model: demote to plain text so it survives.
                    Some(AssistantContent::Text(TextContent::plain(
                        thinking.thinking.clone(),
                    )))
                }
            }
            AssistantContent::Text(text) => {
                if is_same_model {
                    Some(block.clone())
                } else {
                    // Strip the text signature; it references foreign ids.
                    Some(AssistantContent::Text(TextContent::plain(
                        text.text.clone(),
                    )))
                }
            }
            AssistantContent::ToolCall(tool_call) => {
                let mut tool_call = tool_call.clone();
                if !is_same_model {
                    // Google-specific thought signatures don't transfer.
                    tool_call.thought_signature = None;
                    if let Some(normalize) = normalize_tool_call_id {
                        let normalized = normalize(&tool_call.id, model, assistant);
                        if normalized != tool_call.id {
                            tool_call_id_map.insert(tool_call.id.clone(), normalized.clone());
                            tool_call.id = normalized;
                        }
                    }
                }
                Some(AssistantContent::ToolCall(tool_call))
            }
        })
        .collect();

    AssistantMessage {
        content,
        ..assistant.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Api, ImageContent, ModelCost, Provider, Usage, UserMessage};

    fn test_model(supports_images: bool) -> Model {
        Model {
            id: "m1".into(),
            name: "M1".into(),
            api: Api::from(Api::ANTHROPIC_MESSAGES),
            provider: Provider::from(Provider::ANTHROPIC),
            base_url: String::new(),
            reasoning: false,
            thinking_level_map: None,
            input: if supports_images {
                vec![InputModality::Text, InputModality::Image]
            } else {
                vec![InputModality::Text]
            },
            cost: ModelCost {
                input: 0.0,
                output: 0.0,
                cached_read: 0.0,
                cached_write: 0.0,
            },
            context_window: 100_000,
            max_tokens: 4096,
            headers: None,
            compat: None,
        }
    }

    fn assistant_with(content: Vec<AssistantContent>, stop_reason: StopReason) -> Message {
        Message::Assistant(AssistantMessage {
            content,
            api: Api::from(Api::ANTHROPIC_MESSAGES),
            provider: Provider::from(Provider::ANTHROPIC),
            model: "m1".into(),
            response_model: None,
            response_id: None,
            usage: Usage::default(),
            stop_reason,
            error_message: None,
            timestamp: 0,
        })
    }

    #[test]
    fn orphaned_tool_calls_get_synthetic_results() {
        let messages = vec![assistant_with(
            vec![AssistantContent::ToolCall(ToolCall {
                id: "call_1".into(),
                name: "grep".into(),
                arguments: serde_json::json!({}),
                thought_signature: None,
            })],
            StopReason::ToolUse,
        )];
        let out = transform_messages(&messages, &test_model(true), None);
        assert_eq!(out.len(), 2);
        let Message::ToolResult(result) = &out[1] else {
            panic!("expected synthetic tool result");
        };
        assert!(result.is_error);
        assert_eq!(result.tool_call_id, "call_1");
    }

    #[test]
    fn errored_assistant_turns_are_dropped() {
        let messages = vec![assistant_with(
            vec![AssistantContent::Text(TextContent::plain("partial"))],
            StopReason::Error,
        )];
        let out = transform_messages(&messages, &test_model(true), None);
        assert!(out.is_empty());
    }

    #[test]
    fn images_are_downgraded_for_text_only_models() {
        let messages = vec![Message::User(UserMessage {
            content: UserContentBody::Blocks(vec![
                UserContent::Image(ImageContent {
                    data: "abc".into(),
                    mime_type: "image/png".into(),
                }),
                UserContent::Image(ImageContent {
                    data: "def".into(),
                    mime_type: "image/png".into(),
                }),
            ]),
            timestamp: 0,
        })];
        let out = transform_messages(&messages, &test_model(false), None);
        let Message::User(user) = &out[0] else {
            panic!("expected user message");
        };
        // Two consecutive images collapse into ONE placeholder.
        let UserContentBody::Blocks(blocks) = &user.content else {
            panic!("expected blocks");
        };
        assert_eq!(blocks.len(), 1);
    }
}
