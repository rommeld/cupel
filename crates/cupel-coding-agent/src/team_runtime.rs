//! Request-time discussion projection without changing private history or hooks.

use crate::team_conversation::{Participant, TeamRuntime};
use cupel_agent::types::BeforeToolCallResult;
use cupel_agent::{AgentHooks, AgentMessage};
use cupel_core::types::{AssistantMessage, Message, ToolCall, ToolResultMessage};
use std::sync::Arc;

pub struct ConversationHooks {
    inner: Arc<dyn AgentHooks>,
    team: Arc<TeamRuntime>,
    actor: Participant,
}

impl ConversationHooks {
    #[must_use]
    pub fn new(inner: Arc<dyn AgentHooks>, team: Arc<TeamRuntime>, actor: Participant) -> Self {
        Self { inner, team, actor }
    }
}

#[async_trait::async_trait]
impl AgentHooks for ConversationHooks {
    async fn request_messages(&self) -> Vec<AgentMessage> {
        let mut messages = self.inner.request_messages().await;
        match self.team.read(&self.actor, None).await {
            Ok(page) if !page.entries.is_empty() => {
                messages.push(AgentMessage::user_text(page.render(true)));
            }
            Err(problem) if self.actor == Participant::Lead || self.team.permits(&self.actor) => {
                self.team.warn(problem);
            }
            Ok(_) | Err(_) => {}
        }
        messages
    }
    async fn convert_to_llm(&self, messages: &[AgentMessage]) -> Vec<Message> {
        self.inner.convert_to_llm(messages).await
    }
    async fn transform_context(&self, messages: Vec<AgentMessage>) -> Vec<AgentMessage> {
        self.inner.transform_context(messages).await
    }
    async fn api_key(&self, provider: &str) -> Option<String> {
        self.inner.api_key(provider).await
    }
    async fn before_tool_call(
        &self,
        assistant: &AssistantMessage,
        call: &ToolCall,
    ) -> Option<BeforeToolCallResult> {
        self.inner.before_tool_call(assistant, call).await
    }
    async fn should_stop_after_turn(
        &self,
        message: &AssistantMessage,
        results: &[ToolResultMessage],
    ) -> bool {
        self.inner.should_stop_after_turn(message, results).await
    }
    async fn steering_messages(&self) -> Vec<AgentMessage> {
        self.inner.steering_messages().await
    }
    async fn steering_arrived(&self) {
        self.inner.steering_arrived().await;
    }
    async fn follow_up_messages(&self) -> Vec<AgentMessage> {
        self.inner.follow_up_messages().await
    }
}
