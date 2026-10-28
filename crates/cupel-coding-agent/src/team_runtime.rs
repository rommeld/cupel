//! Request-time discussion projection without changing private history or hooks.

use crate::team_conversation::{Participant, TeamRuntime};
use crate::team_scheduler::{CallContext, TeamScheduler};
use cupel_agent::types::BeforeToolCallResult;
use cupel_agent::{AgentHooks, AgentMessage};
use cupel_core::types::{AssistantMessage, Message, ToolCall, ToolResultMessage};
use std::sync::Arc;

pub struct ConversationHooks {
    inner: Arc<dyn AgentHooks>,
    team: Arc<TeamRuntime>,
    actor: Participant,
    scope: Option<(Arc<TeamScheduler>, Arc<CallContext>)>,
}

impl ConversationHooks {
    #[must_use]
    pub fn new(inner: Arc<dyn AgentHooks>, team: Arc<TeamRuntime>, actor: Participant) -> Self {
        Self {
            inner,
            team,
            actor,
            scope: None,
        }
    }

    #[must_use]
    pub fn with_scope(mut self, scheduler: Arc<TeamScheduler>, context: Arc<CallContext>) -> Self {
        self.scope = Some((scheduler, context));
        self
    }
}

#[async_trait::async_trait]
impl AgentHooks for ConversationHooks {
    async fn model_request_problem(&self) -> Option<String> {
        if let Some(problem) = self.inner.model_request_problem().await {
            return Some(problem);
        }
        if let Some((scheduler, context)) = &self.scope
            && let Some(run) = context.current()
        {
            return match run.scope(scheduler).await {
                Ok(scope) => scope.request_problem(),
                Err(problem) => Some(problem),
            };
        }
        None
    }
    fn cancellation_token(&self) -> Option<tokio_util::sync::CancellationToken> {
        self.scope
            .as_ref()
            .and_then(|(_, context)| context.current())
            .map(|run| run.cancellation_token())
            .or_else(|| self.inner.cancellation_token())
    }
    async fn observe_usage(&self, usage: Option<&cupel_core::types::Usage>) {
        self.inner.observe_usage(usage).await;
        if let Some((scheduler, context)) = &self.scope
            && let Some(run) = context.current()
            && let Ok(scope) = run.scope(scheduler).await
        {
            scope.observe_usage(usage);
            scheduler.publish(scope.id()).await;
        }
    }
    async fn request_messages(&self) -> Vec<AgentMessage> {
        let mut messages = self.inner.request_messages().await;
        let mut projection = String::new();
        if let Some((scheduler, context)) = &self.scope
            && let Some(run) = context.current()
        {
            run.observe_start(scheduler).await;
            match run.scope(scheduler).await {
                Ok(scope) => projection = scope.facts(),
                Err(problem) => self.team.warn(problem),
            }
        }
        match self.team.read(&self.actor, None).await {
            Ok(page) if !page.entries.is_empty() => {
                projection.push('\n');
                projection.push_str(&page.render(true));
            }
            Err(problem) if self.actor == Participant::Lead || self.team.permits(&self.actor) => {
                self.team.warn(problem);
            }
            Ok(_) | Err(_) => {}
        }
        if !projection.is_empty() {
            messages.push(AgentMessage::user_text(
                crate::truncate::truncate_head(
                    &projection,
                    crate::truncate::TruncationOptions {
                        max_lines: Some(60),
                        max_bytes: Some(4096),
                    },
                )
                .content,
            ));
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
