//! End-to-end test of history seeding through `AgentOptions.messages` — the
//! path `cupel --resume` uses to restore a persisted transcript. A mock
//! provider captures the `Context` it receives, proving the seeded messages
//! actually reach the LLM request (not just the state snapshot).
#![allow(clippy::tests_outside_test_module)]

use std::sync::{Arc, Mutex};

use futures_util::StreamExt as _;

use cupel_agent::{Agent, AgentEvent, AgentMessage, AgentOptions};
use cupel_core::{
    event_stream::{AssistantMessageStream, assistant_message_channel},
    provider::{Provider, Registry},
    types::{
        Api, AssistantContent, AssistantMessage, Context, Message, Model, ModelCost, StopReason,
        StreamOptions, TextContent, Usage, now_ms,
    },
};

struct CapturingProvider {
    contexts: Arc<Mutex<Vec<Context>>>,
}

impl Provider for CapturingProvider {
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
        let _ = sink.start();
        let message = AssistantMessage {
            content: vec![AssistantContent::Text(TextContent::plain("answer"))],
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

fn seed_messages() -> Vec<AgentMessage> {
    let user = AgentMessage::user_text("earlier question");
    let assistant = AgentMessage::Llm(Message::Assistant(AssistantMessage {
        content: vec![AssistantContent::Text(TextContent::plain("earlier answer"))],
        api: Api::from("mock"),
        provider: cupel_core::types::Provider::from("mock"),
        model: "mock-model".into(),
        response_model: None,
        response_id: None,
        usage: Usage::default(),
        stop_reason: StopReason::Stop,
        error_message: None,
        timestamp: now_ms(),
    }));
    vec![user, assistant]
}

#[tokio::test]
async fn seeded_messages_reach_the_provider_and_survive_in_state() {
    let contexts = Arc::new(Mutex::new(Vec::new()));
    let provider = Arc::new(CapturingProvider {
        contexts: Arc::clone(&contexts),
    });
    let mut registry = Registry::new();
    registry.register(provider);

    let seed = seed_messages();
    let mut options = AgentOptions::new(mock_model(), Arc::new(registry));
    options.api_key = Some("test".into());
    options.messages = seed.clone();
    let mut agent = Agent::new(options);

    assert_eq!(agent.state().messages.len(), 2);

    let mut events = agent.prompt_text("new question").expect("not busy");
    while let Some(event) = events.next().await {
        if matches!(event, AgentEvent::AgentEnd { .. }) {
            break;
        }
    }
    agent.wait_for_idle().await;

    let captured = contexts.lock().unwrap();
    assert_eq!(captured.len(), 1, "exactly one LLM call");
    let sent = &captured[0].messages;
    assert!(sent.len() >= 3, "seed (2) + new prompt, got {}", sent.len());
    let text_of = |m: &Message| match m {
        Message::User(u) => format!("{:?}", u.content),
        Message::Assistant(a) => format!("{:?}", a.content),
        Message::ToolResult(t) => format!("{:?}", t.content),
    };
    assert!(text_of(&sent[0]).contains("earlier question"));
    assert!(text_of(&sent[1]).contains("earlier answer"));
    assert!(text_of(&sent[2]).contains("new question"));

    let state_messages = agent.state().messages;
    assert_eq!(state_messages.len(), 4);
    assert_eq!(state_messages[0..2], seed[..]);
    assert!(matches!(
        &state_messages[2],
        AgentMessage::Llm(Message::User(u)) if format!("{:?}", u.content).contains("new question")
    ));
    assert!(matches!(
        &state_messages[3],
        AgentMessage::Llm(Message::Assistant(a)) if format!("{:?}", a.content).contains("answer")
    ));
}
