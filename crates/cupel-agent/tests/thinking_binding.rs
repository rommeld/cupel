//! End-to-end tests of the agent loop's recovery from a preserved-thinking
//! rejection: after a history edit the provider refuses every replayed
//! thinking block, and the loop retries once without them. A scripted mock
//! provider stands in for Anthropic, so no network is involved.

// Integration-test files under tests/ are compiled as their own crate and
// only ever built in test mode, so the "tests outside #[cfg(test)]"
// restriction lint does not apply here.
#![allow(clippy::tests_outside_test_module)]

use std::sync::{Arc, Mutex};

use futures_util::StreamExt as _;
use tokio_util::sync::CancellationToken;

use cupel_agent::{
    Agent, AgentEvent, AgentHooks, AgentLoopConfig, AgentMessage, AgentOptions, NoHooks,
    RetryConfig, ToolExecutionMode,
    agent_loop::{agent_event_channel, agent_loop},
    types::AgentContext,
};
use cupel_core::{
    event_stream::{AssistantMessageStream, assistant_message_channel},
    provider::{Provider, Registry},
    types::{
        Api, AssistantContent, AssistantMessage, Context, Message, Model, ModelCost, StopReason,
        StreamOptions, TextContent, ThinkingContent, ToolCall, ToolResultContent,
        ToolResultMessage, Usage, now_ms,
    },
};

/// Anthropic's answer to a thinking block replayed after a history edit.
const STALE_BLOCK_ERROR: &str = "provider returned HTTP 400: messages.1.content.0: Invalid `signature` in `thinking` block. The block is bound to a different conversation.";

/// Rejects requests like the real API does after an edit, and records for
/// each request whether it still replayed a thinking block.
struct BindingProvider {
    /// true: reject even requests without thinking blocks (a provider that
    /// keeps failing must not make the loop spin).
    always_reject: bool,
    replayed_thinking: Mutex<Vec<bool>>,
    seen_messages: Mutex<Vec<Vec<Message>>>,
}

impl BindingProvider {
    fn new(always_reject: bool) -> Self {
        Self {
            always_reject,
            replayed_thinking: Mutex::new(Vec::new()),
            seen_messages: Mutex::new(Vec::new()),
        }
    }

    fn replayed_thinking(&self) -> Vec<bool> {
        self.replayed_thinking.lock().expect("lock").clone()
    }
}

fn base_message(model: &Model) -> AssistantMessage {
    AssistantMessage {
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
    }
}

impl Provider for BindingProvider {
    fn api(&self) -> &str {
        "mock"
    }

    fn stream(
        &self,
        model: &Model,
        context: Context,
        _options: StreamOptions,
    ) -> AssistantMessageStream {
        let has_thinking = context.messages.iter().any(|message| match message {
            Message::Assistant(assistant) => assistant
                .content
                .iter()
                .any(|block| matches!(block, AssistantContent::Thinking(_))),
            _ => false,
        });
        self.replayed_thinking
            .lock()
            .expect("lock")
            .push(has_thinking);
        self.seen_messages
            .lock()
            .expect("lock")
            .push(context.messages);

        let (stream, sink) = assistant_message_channel();
        let _ = sink.start();
        if has_thinking || self.always_reject {
            let message = AssistantMessage {
                stop_reason: StopReason::Error,
                error_message: Some(STALE_BLOCK_ERROR.to_string()),
                ..base_message(model)
            };
            let _ = sink.error(StopReason::Error, message);
        } else {
            let message = AssistantMessage {
                content: vec![AssistantContent::Text(TextContent::plain("recovered"))],
                ..base_message(model)
            };
            let _ = sink.done(StopReason::Stop, message);
        }
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
        reasoning: true,
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

/// A finished earlier turn whose thinking block the edit invalidated.
fn earlier_turn(model: &Model) -> Vec<AgentMessage> {
    vec![
        AgentMessage::user_text("earlier question"),
        AgentMessage::Llm(Message::Assistant(AssistantMessage {
            content: vec![
                AssistantContent::Thinking(ThinkingContent {
                    thinking: "reasoning from before the edit".into(),
                    thinking_signature: Some("signature".into()),
                    redacted: None,
                }),
                AssistantContent::Text(TextContent::plain("earlier answer")),
            ],
            ..base_message(model)
        })),
    ]
}

/// Run one prompt on top of the earlier turn, collecting all events.
async fn run_loop_with(provider: Arc<BindingProvider>) -> Vec<AgentEvent> {
    let mut registry = Registry::new();
    registry.register(provider);

    let model = mock_model();
    let context = AgentContext {
        system_prompt: String::new(),
        messages: earlier_turn(&model),
        tools: Vec::new(),
    };
    let config = AgentLoopConfig {
        model,
        thinking_level: None,
        api_key: Some("test".into()),
        session_id: None,
        temperature: None,
        max_tokens: None,
        tool_execution: ToolExecutionMode::Parallel,
        // No transient retries: only the thinking recovery may resend.
        retry: RetryConfig {
            max_retries: 0,
            base_delay_ms: 1,
        },
        compaction: cupel_agent::CompactionConfig::default(),
    };

    let (mut events, sink) = agent_event_channel();
    let loop_task = tokio::spawn(agent_loop(
        vec![AgentMessage::user_text("hello")],
        context,
        config,
        Arc::new(NoHooks),
        Arc::new(registry),
        CancellationToken::new(),
        sink,
    ));

    let mut collected = Vec::new();
    while let Some(event) = events.next().await {
        collected.push(event);
    }
    loop_task.await.expect("loop task completes");
    collected
}

fn last_assistant(events: &[AgentEvent]) -> Option<AssistantMessage> {
    events.iter().rev().find_map(|e| match e {
        AgentEvent::MessageEnd {
            message: AgentMessage::Llm(Message::Assistant(a)),
        } => Some(a.clone()),
        _ => None,
    })
}

#[tokio::test]
async fn stale_thinking_blocks_are_stripped_and_the_turn_retried() {
    let provider = Arc::new(BindingProvider::new(false));
    let events = run_loop_with(Arc::clone(&provider)).await;

    // The first request replayed the stale block and was rejected; the
    // retry went out without it.
    assert_eq!(provider.replayed_thinking(), vec![true, false]);
    let last = last_assistant(&events).expect("final assistant message");
    assert_eq!(last.stop_reason, StopReason::Stop);
    // Not a transient failure: no backoff, no AutoRetry notice.
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, AgentEvent::AutoRetry { .. }))
    );
    let removed: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ThinkingBlocksRemoved { messages } => Some(messages),
            _ => None,
        })
        .collect();
    assert_eq!(removed.len(), 1);
    assert!(removed[0].iter().all(|message| {
        match message {
            AgentMessage::Llm(Message::Assistant(a)) => a
                .content
                .iter()
                .all(|block| !matches!(block, AssistantContent::Thinking(_))),
            _ => true,
        }
    }));
    assert_eq!(removed[0].len(), 4, "seed + prompt + rejection");
}

#[tokio::test]
async fn stripping_happens_once_per_failure_episode() {
    // A provider that keeps rejecting: one strip-and-retry, then the error
    // stands instead of looping forever.
    let provider = Arc::new(BindingProvider::new(true));
    let events = run_loop_with(Arc::clone(&provider)).await;

    assert_eq!(provider.replayed_thinking(), vec![true, false]);
    let last = last_assistant(&events).expect("final assistant message");
    assert_eq!(last.stop_reason, StopReason::Error);
}

#[tokio::test]
async fn thinking_removal_persists_across_runs_even_when_recovery_fails() {
    for always_reject in [false, true] {
        let provider = Arc::new(BindingProvider::new(always_reject));
        let mut registry = Registry::new();
        registry.register(Arc::<BindingProvider>::clone(&provider));
        let mut seed = earlier_turn(&mock_model());
        let tool_call = ToolCall {
            id: "old_call".into(),
            name: "read".into(),
            arguments: serde_json::json!({}),
        };
        let AgentMessage::Llm(Message::Assistant(earlier)) = &mut seed[1] else {
            panic!("seed assistant");
        };
        earlier
            .content
            .push(AssistantContent::Thinking(ThinkingContent {
                thinking: String::new(),
                thinking_signature: Some("encrypted reasoning".into()),
                redacted: Some(true),
            }));
        earlier
            .content
            .push(AssistantContent::ToolCall(tool_call.clone()));
        earlier.stop_reason = StopReason::ToolUse;
        seed.push(AgentMessage::Llm(Message::ToolResult(ToolResultMessage {
            tool_call_id: tool_call.id.clone(),
            tool_name: tool_call.name.clone(),
            content: vec![ToolResultContent::Text(TextContent::plain("file contents"))],
            details: None,
            is_error: false,
            timestamp: now_ms(),
        })));
        let mut options = AgentOptions::new(mock_model(), Arc::new(registry));
        options.messages = seed.clone();
        options.retry.max_retries = 0;
        let mut agent = Agent::new(options);

        let mut events = agent.prompt_text("hello").expect("agent is idle");
        let mut effective = seed.clone();
        let mut removed = 0;
        while let Some(event) = events.next().await {
            match event {
                AgentEvent::MessageEnd { message } => effective.push(message),
                AgentEvent::ThinkingBlocksRemoved { messages } => {
                    effective = messages;
                    removed += 1;
                }
                _ => {}
            }
        }
        agent.wait_for_idle().await;
        assert_eq!(removed, 1);
        let retained = agent.state().messages;
        assert_eq!(retained, effective);
        assert_eq!(retained.len(), seed.len() + 3, "prompt + rejection + retry");
        let AgentMessage::Llm(Message::Assistant(earlier)) = &retained[1] else {
            panic!("retained assistant");
        };
        assert_eq!(
            earlier.content,
            vec![
                AssistantContent::Text(TextContent::plain("earlier answer")),
                AssistantContent::ToolCall(tool_call),
            ]
        );
        assert_eq!(retained[0], seed[0]);
        assert_eq!(retained[2], seed[2], "tool result stays unchanged");

        let mut events = agent.prompt_text("next question").expect("agent is idle");
        while let Some(event) = events.next().await {
            assert!(!matches!(event, AgentEvent::ThinkingBlocksRemoved { .. }));
        }
        agent.wait_for_idle().await;
        assert_eq!(provider.replayed_thinking(), vec![true, false, false]);
        let expected = NoHooks.convert_to_llm(&retained).await;
        let requests = provider.seen_messages.lock().expect("lock");
        assert_eq!(&requests[2][..expected.len()], expected.as_slice());
        assert_eq!(requests[2].len(), expected.len() + 1);
    }
}
