//! Translate visible Cupel events without exposing provider private reasoning.

use agent_client_protocol::schema::v1 as acp;
use cupel_agent::{AgentEvent, AgentMessage};
use cupel_core::types::{
    AssistantContent, AssistantMessageEvent, Message, StopReason, ToolResultContent,
    UserContentBody,
};

use crate::session::failure;

#[derive(Default)]
pub struct Mapper {
    streamed_text: bool,
    result: Option<acp::Result<acp::StopReason>>,
}

fn text_chunk(text: &str) -> acp::ContentChunk {
    acp::ContentChunk::new(acp::ContentBlock::Text(acp::TextContent::new(text)))
}

fn tool_kind(name: &str) -> acp::ToolKind {
    match name {
        "read" => acp::ToolKind::Read,
        "grep" => acp::ToolKind::Search,
        "apply_patch" => acp::ToolKind::Edit,
        "bash" => acp::ToolKind::Execute,
        _ => acp::ToolKind::Other,
    }
}

fn output(content: &[ToolResultContent]) -> Vec<acp::ToolCallContent> {
    content
        .iter()
        .filter_map(|item| match item {
            ToolResultContent::Text(text) => Some(acp::ToolCallContent::Content(
                acp::Content::new(acp::ContentBlock::Text(acp::TextContent::new(&text.text))),
            )),
            ToolResultContent::Image(_) => None,
        })
        .collect()
}

fn tool_update(id: &str, fields: acp::ToolCallUpdateFields) -> acp::SessionUpdate {
    acp::SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(id.to_owned(), fields))
}

/// Replay finalized visible history, never run tools or record it a second time.
#[must_use]
pub fn replay(message: &AgentMessage) -> Vec<acp::SessionUpdate> {
    match message {
        AgentMessage::Llm(Message::User(user)) => match &user.content {
            UserContentBody::Text(text) => {
                vec![acp::SessionUpdate::UserMessageChunk(text_chunk(text))]
            }
            UserContentBody::Blocks(blocks) => blocks
                .iter()
                .filter_map(|block| match block {
                    cupel_core::types::UserContent::Text(text) => {
                        Some(acp::SessionUpdate::UserMessageChunk(text_chunk(&text.text)))
                    }
                    cupel_core::types::UserContent::Image(_) => None,
                })
                .collect(),
        },
        AgentMessage::Llm(Message::Assistant(assistant)) => assistant
            .content
            .iter()
            .filter_map(|block| match block {
                AssistantContent::Text(text) => Some(acp::SessionUpdate::AgentMessageChunk(
                    text_chunk(&text.text),
                )),
                AssistantContent::ToolCall(call) => Some(acp::SessionUpdate::ToolCall(
                    acp::ToolCall::new(call.id.clone(), call.name.clone())
                        .kind(tool_kind(&call.name))
                        .raw_input(call.arguments.clone()),
                )),
                AssistantContent::Thinking(_) => None,
            })
            .collect(),
        AgentMessage::Llm(Message::ToolResult(result)) => vec![tool_update(
            &result.tool_call_id,
            acp::ToolCallUpdateFields::new()
                .status(if result.is_error {
                    acp::ToolCallStatus::Failed
                } else {
                    acp::ToolCallStatus::Completed
                })
                .content(output(&result.content)),
        )],
        AgentMessage::Custom { .. } => Vec::new(),
    }
}

impl Mapper {
    pub fn event(&mut self, event: &AgentEvent) -> Vec<acp::SessionUpdate> {
        match event {
            AgentEvent::TurnStart => {
                self.streamed_text = false;
                Vec::new()
            }
            AgentEvent::MessageUpdate {
                event: AssistantMessageEvent::TextDelta { delta, .. },
            } => {
                self.streamed_text = true;
                vec![acp::SessionUpdate::AgentMessageChunk(text_chunk(delta))]
            }
            AgentEvent::MessageEnd {
                message: AgentMessage::Llm(Message::Assistant(assistant)),
            } => {
                self.result = Some(match assistant.stop_reason {
                    StopReason::Stop | StopReason::ToolUse => Ok(acp::StopReason::EndTurn),
                    StopReason::Length => Ok(acp::StopReason::MaxTokens),
                    StopReason::Error | StopReason::Aborted => Err(failure(
                        assistant
                            .error_message
                            .as_deref()
                            .unwrap_or("model request failed"),
                    )),
                });
                replay(&AgentMessage::Llm(Message::Assistant(assistant.clone())))
                    .into_iter()
                    .filter(|update| {
                        !self.streamed_text
                            || !matches!(update, acp::SessionUpdate::AgentMessageChunk(_))
                    })
                    .collect()
            }
            AgentEvent::ToolExecutionStart { tool_call_id, .. } => vec![tool_update(
                tool_call_id,
                acp::ToolCallUpdateFields::new().status(acp::ToolCallStatus::InProgress),
            )],
            AgentEvent::ToolExecutionUpdate {
                tool_call_id,
                partial,
                ..
            } => vec![tool_update(
                tool_call_id,
                acp::ToolCallUpdateFields::new().content(output(&partial.content)),
            )],
            AgentEvent::MessageEnd {
                message: message @ AgentMessage::Llm(Message::ToolResult(_)),
            } => replay(message),
            _ => Vec::new(),
        }
    }

    pub fn finish(self, cancelled: bool) -> acp::Result<acp::PromptResponse> {
        let reason = if cancelled {
            acp::StopReason::Cancelled
        } else {
            self.result
                .ok_or_else(|| failure("agent ended without an assistant result"))??
        };
        Ok(acp::PromptResponse::new(reason))
    }
}
