#![allow(clippy::tests_outside_test_module)]

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use futures_util::StreamExt as _;

use cupel_agent::{Agent, AgentEvent, AgentMessage, AgentOptions};
use cupel_core::{
    event_stream::{AssistantMessageStream, assistant_message_channel},
    provider::{Provider, Registry},
    types::{
        Api, AssistantContent, AssistantMessage, Context, Message, Model, ModelCost, StopReason,
        StreamOptions, TextContent, Usage, UserContentBody, now_ms,
    },
};

struct NumberingProvider {
    calls: AtomicU32,
    contexts: Arc<Mutex<Vec<Context>>>,
}

impl Provider for NumberingProvider {
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
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let (stream, sink) = assistant_message_channel();
        let _ = sink.start();
        let message = AssistantMessage {
            content: vec![AssistantContent::Text(TextContent::plain(format!(
                "answer {call}"
            )))],
            api: model.api.clone(),
            provider: model.provider.clone(),
            model: model.id.clone(),
            response_model: None,
            response_id: None,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            error_kind: None,
            timestamp: now_ms(),
        };
        let _ = sink.done(StopReason::Stop, message);
        stream
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

fn describe(message: &Message) -> String {
    match message {
        Message::User(user) => match &user.content {
            UserContentBody::Text(text) => format!("user:{text}"),
            UserContentBody::Blocks(_) => "user:(blocks)".to_string(),
        },
        Message::Assistant(assistant) => {
            let text: String = assistant
                .content
                .iter()
                .filter_map(|content| match content {
                    AssistantContent::Text(t) => Some(t.text.as_str()),
                    _ => None,
                })
                .collect();
            format!("assistant:{text}")
        }
        Message::ToolResult(_) => "tool_result".to_string(),
    }
}

#[tokio::test]
async fn queued_messages_run_one_at_a_time_inside_the_same_run() {
    let contexts = Arc::new(Mutex::new(Vec::new()));
    let mut registry = Registry::new();
    registry.register(Arc::new(NumberingProvider {
        calls: AtomicU32::new(0),
        contexts: Arc::clone(&contexts),
    }));
    let mut options = AgentOptions::new(mock_model(), Arc::new(registry));
    options.api_key = Some("test".into());
    let mut agent = Agent::new(options);

    agent.follow_up(AgentMessage::user_text("second"));
    agent.follow_up(AgentMessage::user_text("third"));

    let mut events = agent.prompt_text("first").expect("not busy");
    let mut ended = Vec::new();
    let mut agent_ends = 0;
    while let Some(event) = events.next().await {
        match event {
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
        vec![
            "user:first",
            "assistant:answer 0",
            "user:second",
            "assistant:answer 1",
            "user:third",
            "assistant:answer 2",
        ]
    );
    assert_eq!(agent_ends, 1, "follow-ups continue the run");
    let sent = contexts.lock().unwrap();
    assert_eq!(sent.len(), 3);
    assert_eq!(describe(sent[1].messages.last().unwrap()), "user:second");
    assert_eq!(describe(sent[2].messages.last().unwrap()), "user:third");
    assert!(agent.take_follow_up().is_none(), "queue drained");
}

#[tokio::test]
async fn a_failed_run_leaves_its_follow_ups_queued() {
    let registry = Arc::new(Registry::new());
    let mut agent = Agent::new(AgentOptions::new(mock_model(), registry));
    agent.follow_up(AgentMessage::user_text("later"));

    let mut events = agent.prompt_text("now").expect("not busy");
    while events.next().await.is_some() {}
    agent.wait_for_idle().await;

    let left = agent.take_follow_up().expect("still queued");
    assert!(matches!(&left, AgentMessage::Llm(message) if describe(message) == "user:later"));
    assert!(agent.take_follow_up().is_none());
}
