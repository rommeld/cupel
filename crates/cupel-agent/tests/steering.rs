//! Steering against a scripted provider: a message steered in while the
//! model streams cuts that reply short, one steered in while a tool runs
//! waits for the tool and interrupts nothing.

// Integration-test files under tests/ are compiled as their own crate and
// only ever built in test mode, so the "tests outside #[cfg(test)]"
// restriction lint does not apply here.
#![allow(clippy::tests_outside_test_module)]

use std::sync::{Arc, Mutex};

use futures_util::StreamExt as _;
use tokio_util::sync::CancellationToken;

use cupel_agent::{
    Agent, AgentEvent, AgentMessage, AgentOptions,
    types::{AgentTool, AgentToolResult, ToolError, ToolUpdateFn},
};
use cupel_core::{
    event_stream::{AssistantMessageStream, assistant_message_channel},
    provider::{Provider, Registry},
    types::{
        Api, AssistantContent, AssistantMessage, Context, Message, Model, ModelCost, StopReason,
        StreamOptions, TextContent, ToolCall, Usage, UserContentBody, now_ms,
    },
};

/// What the provider does on one call.
#[derive(Clone, Copy)]
enum Reply {
    /// Stream `n` words, 10 ms apart, and stop early when the request's
    /// signal fires, the way a real provider does.
    Slow(usize),
    /// Ask for the `wait` tool.
    Tool,
}

/// A provider that plays one scripted reply per call and records every
/// context it was sent.
struct ScriptedProvider {
    replies: Mutex<Vec<Reply>>,
    contexts: Arc<Mutex<Vec<Context>>>,
}

fn message(model: &Model, content: Vec<AssistantContent>, stop: StopReason) -> AssistantMessage {
    AssistantMessage {
        content,
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        response_model: None,
        response_id: None,
        usage: Usage::default(),
        stop_reason: stop,
        error_message: (stop == StopReason::Aborted).then(|| "request was aborted".to_string()),
        error_kind: None,
        timestamp: now_ms(),
    }
}

impl Provider for ScriptedProvider {
    fn api(&self) -> &str {
        "mock"
    }

    fn stream(
        &self,
        model: &Model,
        context: Context,
        options: StreamOptions,
    ) -> AssistantMessageStream {
        self.contexts.lock().unwrap().push(context);
        let reply = self.replies.lock().unwrap().remove(0);
        let (stream, sink) = assistant_message_channel();
        let model = model.clone();
        let signal = options.signal.unwrap_or_default();
        tokio::spawn(async move {
            let _ = sink.start();
            match reply {
                Reply::Tool => {
                    let call = AssistantContent::ToolCall(ToolCall {
                        id: "call_1".into(),
                        name: "wait".into(),
                        arguments: serde_json::json!({}),
                    });
                    let done = message(&model, vec![call], StopReason::ToolUse);
                    let _ = sink.done(StopReason::ToolUse, done);
                }
                Reply::Slow(words) => {
                    let mut text = String::new();
                    for _ in 0..words {
                        tokio::select! {
                            () = signal.cancelled() => {
                                let content = vec![AssistantContent::Text(TextContent::plain(text))];
                                let aborted = message(&model, content, StopReason::Aborted);
                                let _ = sink.error(StopReason::Aborted, aborted);
                                return;
                            }
                            () = tokio::time::sleep(std::time::Duration::from_millis(10)) => {}
                        }
                        text.push_str("word ");
                        let _ = sink.text_delta(0, "word ".to_string());
                    }
                    let content = vec![AssistantContent::Text(TextContent::plain(text))];
                    let _ = sink.done(StopReason::Stop, message(&model, content, StopReason::Stop));
                }
            }
        });
        stream
    }
}

/// A tool that takes 100 ms and reports whether it was cancelled.
struct WaitTool;

#[async_trait::async_trait]
impl AgentTool for WaitTool {
    fn name(&self) -> &str {
        "wait"
    }
    fn description(&self) -> &str {
        "wait"
    }
    fn parameters(&self) -> serde_json::Value {
        serde_json::json!({"type": "object"})
    }
    async fn execute(
        &self,
        _tool_call_id: &str,
        _args: serde_json::Value,
        cancel: CancellationToken,
        _on_update: Option<ToolUpdateFn>,
    ) -> Result<AgentToolResult, ToolError> {
        tokio::select! {
            () = cancel.cancelled() => Ok(AgentToolResult::text("cancelled")),
            () = tokio::time::sleep(std::time::Duration::from_millis(100)) => {
                Ok(AgentToolResult::text("finished"))
            }
        }
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
        input: vec![cupel_core::types::InputModality::Text],
        cost: ModelCost::default(),
        context_window: 100_000,
        max_context_window: None,
        max_tokens: 4096,
        headers: None,
        compat: None,
    }
}

/// An agent on the scripted provider, plus the contexts it will record.
fn agent_with(replies: Vec<Reply>) -> (Agent, Arc<Mutex<Vec<Context>>>) {
    let contexts = Arc::new(Mutex::new(Vec::new()));
    let mut registry = Registry::new();
    registry.register(Arc::new(ScriptedProvider {
        replies: Mutex::new(replies),
        contexts: Arc::clone(&contexts),
    }));
    let mut options = AgentOptions::new(mock_model(), Arc::new(registry));
    options.api_key = Some("test".into());
    options.tools = vec![Arc::new(WaitTool)];
    (Agent::new(options), contexts)
}

/// One short line per message, for readable assertions.
fn describe(message: &Message) -> String {
    match message {
        Message::User(user) => match &user.content {
            UserContentBody::Text(text) => format!("user:{text}"),
            UserContentBody::Blocks(_) => "user:(blocks)".to_string(),
        },
        Message::Assistant(assistant) => format!("assistant:{:?}", assistant.stop_reason),
        Message::ToolResult(result) => format!("tool:{:?}", result.content),
    }
}

#[tokio::test]
async fn steering_cuts_a_streaming_reply_short() {
    let (mut agent, contexts) = agent_with(vec![Reply::Slow(500), Reply::Slow(3)]);
    let mut events = agent.prompt_text("build it").expect("not busy");

    let mut ended = Vec::new();
    let mut agent_ends = 0;
    let mut steered = false;
    while let Some(event) = events.next().await {
        match event {
            // The first word proves the reply is streaming: steer now.
            AgentEvent::MessageUpdate { .. } if !steered => {
                agent.steer(AgentMessage::user_text("use the other file"));
                steered = true;
            }
            AgentEvent::MessageEnd {
                message: AgentMessage::Llm(message),
            } => ended.push(describe(&message)),
            AgentEvent::AgentEnd { .. } => agent_ends += 1,
            _ => {}
        }
    }
    agent.wait_for_idle().await;

    assert_eq!(
        ended,
        [
            "user:build it",
            "assistant:Aborted",
            "user:use the other file",
            "assistant:Stop",
        ]
    );
    assert_eq!(agent_ends, 1, "steering continues the run");
    let sent = contexts.lock().unwrap();
    assert_eq!(sent.len(), 2);
    let last = sent[1].messages.last().expect("a message");
    assert_eq!(describe(last), "user:use the other file");
    assert!(agent.take_steering().is_empty(), "queue drained");
}

#[tokio::test]
async fn steering_waits_for_a_running_tool_and_interrupts_nothing() {
    let (mut agent, contexts) = agent_with(vec![Reply::Tool, Reply::Slow(5)]);
    let mut events = agent.prompt_text("build it").expect("not busy");

    let mut ended = Vec::new();
    while let Some(event) = events.next().await {
        match event {
            AgentEvent::ToolExecutionStart { .. } => {
                agent.steer(AgentMessage::user_text("also add a test"));
            }
            AgentEvent::MessageEnd {
                message: AgentMessage::Llm(message),
            } => ended.push(describe(&message)),
            _ => {}
        }
    }
    agent.wait_for_idle().await;

    // The tool ran to the end, the steering message followed its result,
    // and the next reply finished: the signal left behind by `steer` did
    // not cut it short once its message was delivered.
    assert_eq!(
        ended,
        [
            "user:build it",
            "assistant:ToolUse",
            r#"tool:[Text(TextContent { text: "finished", text_signature: None })]"#,
            "user:also add a test",
            "assistant:Stop",
        ]
    );
    assert_eq!(contexts.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn abort_still_ends_a_steered_run() {
    let (mut agent, contexts) = agent_with(vec![Reply::Slow(500), Reply::Slow(3)]);
    let mut events = agent.prompt_text("build it").expect("not busy");

    let mut ended = Vec::new();
    let mut steered = false;
    while let Some(event) = events.next().await {
        match event {
            // Steer and abort at the same moment: abort wins.
            AgentEvent::MessageUpdate { .. } if !steered => {
                agent.steer(AgentMessage::user_text("use the other file"));
                agent.abort();
                steered = true;
            }
            AgentEvent::MessageEnd {
                message: AgentMessage::Llm(message),
            } => ended.push(describe(&message)),
            _ => {}
        }
    }
    agent.wait_for_idle().await;

    assert_eq!(ended, ["user:build it", "assistant:Aborted"]);
    assert_eq!(contexts.lock().unwrap().len(), 1, "no second request");
    // The run never took the message. A frontend hands it back to the user.
    assert_eq!(agent.take_steering().len(), 1);
}
