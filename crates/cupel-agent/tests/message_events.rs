#![allow(clippy::tests_outside_test_module)]

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use cupel_core::types::{Model, ModelCost};
use futures_util::StreamExt as _;

use cupel_agent::{
    Agent, AgentEvent, AgentMessage, AgentOptions,
    types::{AgentTool, AgentToolResult, ToolError, ToolUpdateFn},
};
use cupel_core::{
    event_stream::{AssistantMessageStream, assistant_message_channel},
    provider::{Provider, Registry},
    types::{
        Api, AssistantContent, AssistantMessage, Context, InputModality, Message, StopReason,
        StreamOptions, TextContent, ToolCall, ToolResultContent, Usage, now_ms,
    },
};
use tokio_util::sync::CancellationToken;

struct ToolThenTextProvider {
    calls: AtomicU32,
    contexts: Arc<Mutex<Vec<Context>>>,
}

impl Provider for ToolThenTextProvider {
    fn api(&self) -> &str {
        "mock"
    }

    fn stream(
        &self,
        model: &Model,
        context: Context,
        _options: StreamOptions,
    ) -> AssistantMessageStream {
        self.contexts.lock().unwrap().push(context);
        let (stream, sink) = assistant_message_channel();
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let base = AssistantMessage {
            content: Vec::new(),
            api: model.api.clone(),
            provider: model.provider.clone(),
            model: model.id.clone(),
            response_model: None,
            response_id: None,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: now_ms(),
        };
        let _ = sink.start();
        if call == 0 {
            let message = AssistantMessage {
                content: vec![AssistantContent::ToolCall(ToolCall {
                    id: "call_1".into(),
                    name: "echo".into(),
                    arguments: serde_json::json!({"text": "ping"}),
                })],
                stop_reason: StopReason::ToolUse,
                ..base
            };
            let _ = sink.done(StopReason::ToolUse, message);
        } else {
            let message = AssistantMessage {
                content: vec![AssistantContent::Text(TextContent::plain("done"))],
                ..base
            };
            let _ = sink.done(StopReason::Stop, message);
        }
        stream
    }
}

struct EchoTool;

#[async_trait::async_trait]
impl AgentTool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }
    fn description(&self) -> &str {
        "echo"
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    async fn execute(
        &self,
        _tool_call_id: &str,
        args: serde_json::Value,
        _cancel: CancellationToken,
        on_update: Option<ToolUpdateFn>,
    ) -> Result<AgentToolResult, ToolError> {
        let text = args["text"].as_str().unwrap_or("").to_string();
        if let Some(on_update) = &on_update {
            on_update(AgentToolResult::text("echo: "));
        }
        Ok(AgentToolResult::text(format!("echo: {text}")))
    }
}

fn mock_model() -> Model {
    Model {
        id: "mock-model".into(),
        name: "Mock".into(),
        api: Api::from("mock"),
        provider: cupel_core::types::Provider::from("mock"),
        base_url: String::new(),
        reasoning: false,
        thinking_level_map: None,
        input: vec![InputModality::Text],
        cost: ModelCost::default(),
        context_window: 100_000,
        max_context_window: None,
        max_tokens: 4096,
        headers: None,
        compat: None,
    }
}

fn describe(message: &Message) -> String {
    match message {
        Message::User(u) => format!("user:{:?}", u.content),
        Message::Assistant(a) => format!(
            "assistant:{}",
            a.content
                .iter()
                .map(|c| match c {
                    AssistantContent::Text(t) => t.text.clone(),
                    AssistantContent::ToolCall(tc) => format!("call {}", tc.name),
                    AssistantContent::Thinking(_) => "thinking".into(),
                })
                .collect::<Vec<_>>()
                .join("+")
        ),
        Message::ToolResult(t) => format!(
            "tool_result:{}:{}",
            t.tool_name,
            t.content
                .iter()
                .filter_map(|c| match c {
                    ToolResultContent::Text(text) => Some(text.text.clone()),
                    ToolResultContent::Image(_) => None,
                })
                .collect::<String>()
        ),
    }
}

async fn two_turns() -> (Vec<AgentEvent>, Vec<Context>) {
    let contexts = Arc::new(Mutex::new(Vec::new()));
    let mut registry = Registry::new();
    registry.register(Arc::new(ToolThenTextProvider {
        calls: AtomicU32::new(0),
        contexts: Arc::clone(&contexts),
    }));
    let mut options = AgentOptions::new(mock_model(), Arc::new(registry));
    options.api_key = Some("test".into());
    options.tools = vec![Arc::new(EchoTool) as Arc<dyn AgentTool>];
    let mut agent = Agent::new(options);

    let mut events = agent.prompt_text("first question").unwrap();
    let mut first_run = Vec::new();
    while let Some(event) = events.next().await {
        first_run.push(event);
    }
    agent.wait_for_idle().await;

    let mut events = agent.prompt_text("second question").unwrap();
    while events.next().await.is_some() {}
    agent.wait_for_idle().await;

    let seen = contexts.lock().unwrap().clone();
    (first_run, seen)
}

#[tokio::test]
async fn second_turn_replays_the_whole_first_turn() {
    let (_, contexts) = two_turns().await;
    assert_eq!(contexts.len(), 3);

    let third: Vec<String> = contexts[2].messages.iter().map(describe).collect();
    assert_eq!(
        third,
        vec![
            "user:Text(\"first question\")",
            "assistant:call echo",
            "tool_result:echo:echo: ping",
            "assistant:done",
            "user:Text(\"second question\")",
        ],
        "the provider must see the complete first turn, got: \n{third:#?}"
    );
}

fn result_text(result: &AgentToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|c| match c {
            ToolResultContent::Text(t) => Some(t.text.as_str()),
            ToolResultContent::Image(_) => None,
        })
        .collect()
}

#[tokio::test]
async fn tool_execution_announces_start_progress_and_end() {
    let (events, _) = two_turns().await;
    let lifecycle: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::ToolExecutionStart { tool_call_id, .. } => {
                Some(format!("start {tool_call_id}"))
            }
            AgentEvent::ToolExecutionUpdate {
                tool_call_id,
                partial,
                ..
            } => Some(format!("update {tool_call_id} {:?}", result_text(partial))),
            AgentEvent::ToolExecutionEnd { tool_call_id, .. } => {
                Some(format!("end {tool_call_id}"))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        lifecycle,
        vec!["start call_1", "update call_1 \"echo: \"", "end call_1"]
    );
}

#[tokio::test]
async fn every_message_of_a_run_arrives_as_message_end() {
    let (events, _) = two_turns().await;
    let ended: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::MessageEnd {
                message: AgentMessage::Llm(message),
            } => Some(describe(message)),
            _ => None,
        })
        .collect();
    assert_eq!(
        ended,
        vec![
            "user:Text(\"first question\")",
            "assistant:call echo",
            "tool_result:echo:echo: ping",
            "assistant:done",
        ]
    );
    let ends = events
        .iter()
        .filter(|e| matches!(e, AgentEvent::AgentEnd { .. }))
        .count();
    assert_eq!(ends, 1);
}
