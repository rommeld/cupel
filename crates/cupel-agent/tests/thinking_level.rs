//! "Thinking off" on a model that cannot turn reasoning off, the request
//! carries the model's lowest level, not a missing setting.

// Integration test files under tests/ are compiled as their own crate and
// only ever built in test mode, so the "tests outside #[cfg(test)]"
// restriction lint does not apply here.
#![allow(clippy::tests_outside_test_module)]

use std::sync::{Arc, Mutex};

use futures_util::StreamExt as _;

use cupel_agent::{Agent, AgentOptions};
use cupel_core::{
    event_stream::{AssistantMessageStream, assistant_message_channel},
    provider::{Provider, Registry},
    types::{
        Api, AssistantMessage, Context, Model, ModelCost, StopReason, StreamOptions, ThinkingLevel,
        Usage, now_ms,
    },
};

/// Answers at once and records the reasoning level of every request.
struct RecordingProvider {
    levels: Arc<Mutex<Vec<Option<ThinkingLevel>>>>,
}

impl Provider for RecordingProvider {
    fn api(&self) -> &str {
        "mock"
    }

    fn stream(
        &self,
        model: &Model,
        _context: Context,
        options: StreamOptions,
    ) -> AssistantMessageStream {
        self.levels.lock().unwrap().push(options.reasoning);
        let (stream, sink) = assistant_message_channel();
        let _ = sink.start();
        let message = AssistantMessage {
            content: Vec::new(),
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

/// A reasoning model whose catalog row maps "off" to null.
fn always_reasoning_model() -> Model {
    Model {
        id: "mock-model".into(),
        name: "Mock".into(),
        api: Api::from("mock"),
        provider: cupel_core::types::Provider::from("mock"),
        base_url: String::new(),
        reasoning: true,
        thinking_level_map: Some([("off".to_string(), None)].into_iter().collect()),
        input: vec![cupel_core::types::InputModality::Text],
        cost: ModelCost::default(),
        context_window: 100_000,
        max_context_window: None,
        max_tokens: 4096,
        headers: None,
        compat: None,
    }
}

#[tokio::test]
async fn thinking_off_sends_the_lowest_level_where_off_does_not_exist() {
    let levels = Arc::new(Mutex::new(Vec::new()));
    let mut registry = Registry::new();
    registry.register(Arc::new(RecordingProvider {
        levels: Arc::clone(&levels),
    }));
    let mut options = AgentOptions::new(always_reasoning_model(), Arc::new(registry));
    options.api_key = Some("test".into());
    let mut agent = Agent::new(options);
    agent.set_thinking_level(None);

    let mut events = agent.prompt_text("hello").expect("not busy");
    while events.next().await.is_some() {}
    agent.wait_for_idle().await;

    assert_eq!(*levels.lock().unwrap(), [Some(ThinkingLevel::Minimal)]);
    // The request changed, the wish did not.
    assert_eq!(agent.thinking_level(), None);
    assert_eq!(
        agent.effective_thinking_level(),
        Some(ThinkingLevel::Minimal)
    );
}
