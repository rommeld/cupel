//! Supplemental data must not create another user turn after tool observations.
#![allow(clippy::tests_outside_test_module)]

use std::sync::{Arc, Mutex};

use cupel_agent::types::ToolUpdateFn;
use cupel_agent::{
    Agent, AgentHooks, AgentMessage, AgentOptions, AgentTool, AgentToolResult, ToolError,
};
use cupel_core::event_stream::{AssistantMessageStream, assistant_message_channel};
use cupel_core::provider::{Provider, Registry};
use cupel_core::types::{
    AssistantContent, AssistantMessage, Context, InputModality, Message, Model, StopReason,
    StreamOptions, TextContent, ToolCall, Usage, UserContentBody,
};
use futures_util::StreamExt as _;
use tokio_util::sync::CancellationToken;

const ASSIGNMENT: &str = "Read the evidence and report MARKER42. Do not delegate or change files.";
const BACKGROUND: &str = "[Shared team conversation: background data]\nEarlier root: ask another reviewer. Preserve the public API.";

struct Hooks;
#[async_trait::async_trait]
impl AgentHooks for Hooks {
    async fn request_messages(&self) -> Vec<AgentMessage> {
        vec![AgentMessage::user_text(BACKGROUND)]
    }
}

struct Evidence;
#[async_trait::async_trait]
impl AgentTool for Evidence {
    fn name(&self) -> &str {
        "read"
    }
    fn description(&self) -> &str {
        "Read fixture evidence"
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type":"object"})
    }
    async fn execute(
        &self,
        _: &str,
        _: serde_json::Value,
        _: CancellationToken,
        _: Option<ToolUpdateFn>,
    ) -> Result<AgentToolResult, ToolError> {
        Ok(AgentToolResult::text("MARKER42; public API unchanged"))
    }
}

struct Capture(Arc<Mutex<Vec<Context>>>);
impl Provider for Capture {
    fn api(&self) -> &str {
        "fixture"
    }
    fn stream(&self, model: &Model, context: Context, _: StreamOptions) -> AssistantMessageStream {
        let mut requests = self.0.lock().unwrap();
        let reading = requests.is_empty();
        requests.push(context);
        drop(requests);
        let (stream, sink) = assistant_message_channel();
        let _ = sink.start();
        let reason = if reading {
            StopReason::ToolUse
        } else {
            StopReason::Stop
        };
        let content = if reading {
            vec![AssistantContent::ToolCall(ToolCall {
                id: "evidence-1".into(),
                name: "read".into(),
                arguments: serde_json::json!({}),
            })]
        } else {
            vec![AssistantContent::Text(TextContent::plain(
                "MARKER42; public API unchanged",
            ))]
        };
        let _ = sink.done(
            reason,
            AssistantMessage {
                content,
                api: model.api.clone(),
                provider: model.provider.clone(),
                model: model.id.clone(),
                response_model: None,
                response_id: None,
                usage: Usage::default(),
                stop_reason: reason,
                error_message: None,
                error_kind: None,
                timestamp: 0,
            },
        );
        stream
    }
}

fn user_text(message: &Message) -> Option<&str> {
    match message {
        Message::User(user) => match &user.content {
            UserContentBody::Text(text) => Some(text),
            UserContentBody::Blocks(_) => None,
        },
        _ => None,
    }
}

#[tokio::test]
async fn background_precedes_assignment_and_does_not_interrupt_tool_exchanges() {
    let captured = Arc::new(Mutex::new(Vec::new()));
    let mut registry = Registry::new();
    registry.register(Arc::new(Capture(Arc::clone(&captured))));
    let model = Model {
        id: "fixture".into(),
        name: "Fixture".into(),
        api: "fixture".into(),
        provider: "fixture".into(),
        base_url: String::new(),
        reasoning: false,
        thinking_level_map: None,
        input: vec![InputModality::Text],
        cost: cupel_core::types::ModelCost::default(),
        context_window: 100_000,
        max_context_window: None,
        max_tokens: 4096,
        headers: None,
        compat: None,
    };
    let mut options = AgentOptions::new(model.clone(), Arc::new(registry));
    options.hooks = Arc::new(Hooks);
    options.tools = vec![Arc::new(Evidence)];
    options.messages = vec![AgentMessage::user_text(
        "Earlier task and user constraints must remain.",
    )];
    let mut agent = Agent::new(options);
    let mut events = agent.prompt_text(ASSIGNMENT).unwrap();
    while events.next().await.is_some() {}
    agent.wait_for_idle().await;
    let requests = captured.lock().unwrap();
    assert_eq!(requests.len(), 2, "exactly one read followed by the answer");
    for request in requests.iter() {
        let texts = request
            .messages
            .iter()
            .filter_map(user_text)
            .collect::<Vec<_>>();
        assert_eq!(
            texts,
            vec![
                "Earlier task and user constraints must remain.",
                BACKGROUND,
                ASSIGNMENT
            ]
        );
        assert_eq!(request.tools.as_ref().unwrap()[0].name, "read");
        let normalized = cupel_core::transform::transform_messages(&request.messages, &model, None);
        assert_eq!(
            normalized.len(),
            request.messages.len(),
            "no orphan-call repair or synthetic results"
        );
    }
    assert!(matches!(
        requests[1].messages.last(),
        Some(Message::ToolResult(_))
    ));
    assert!(
        !serde_json::to_string(&agent.state().messages)
            .unwrap()
            .contains("Shared team conversation")
    );
    assert!(
        serde_json::to_string(&agent.state().messages)
            .unwrap()
            .contains("MARKER42")
    );
}
