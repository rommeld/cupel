//! End-to-end tests of context compaction, using a mock provider that
//! serves BOTH request kinds the loop makes: summarization calls
//! (recognized by the summarization system prompt) and normal turn calls.

// Integration-test files are compiled as their own crate in test mode; the
// "tests outside #[cfg(test)]" restriction lint does not apply.
#![allow(clippy::tests_outside_test_module)]

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use futures_util::StreamExt as _;
use tokio_util::sync::CancellationToken;

use cupel_agent::{
    AgentEvent, AgentLoopConfig, AgentMessage, CompactionConfig, CompactionReason, NoHooks,
    RetryConfig, ToolExecutionMode,
    agent_loop::{agent_event_channel, agent_loop},
    compaction::{COMPACTION_MARKER, SUMMARIZATION_SYSTEM_PROMPT},
    types::AgentContext,
};
use cupel_core::{
    event_stream::{AssistantMessageStream, assistant_message_channel},
    provider::{Provider, Registry},
    types::{
        Api, AssistantContent, AssistantMessage, Context, Message, Model, ModelCost, StopReason,
        StreamOptions, TextContent, Usage, UserContentBody, now_ms,
    },
};

/// Serves summarization requests with a fixed summary; records every TURN
/// request's message list. Optionally fails the first N turn requests.
struct CompactionAwareProvider {
    turn_calls: AtomicU32,
    fail_first_turns: u32,
    turn_error: &'static str,
    /// First-message text + message count of each turn request, for asserts.
    seen_turn_requests: Mutex<Vec<(String, usize)>>,
}

impl CompactionAwareProvider {
    fn new(fail_first_turns: u32, turn_error: &'static str) -> Self {
        Self {
            turn_calls: AtomicU32::new(0),
            fail_first_turns,
            turn_error,
            seen_turn_requests: Mutex::new(Vec::new()),
        }
    }
}

fn assistant(model: &Model, content: Vec<AssistantContent>) -> AssistantMessage {
    AssistantMessage {
        content,
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        response_model: None,
        response_id: None,
        usage: Usage::default(),
        stop_reason: StopReason::Stop,
        error_message: None,
        timestamp: now_ms(),
    }
}

impl Provider for CompactionAwareProvider {
    fn api(&self) -> &str {
        "mock"
    }

    fn stream(
        &self,
        model: &Model,
        context: Context,
        _options: StreamOptions,
    ) -> AssistantMessageStream {
        let (stream, sink) = assistant_message_channel();
        let _ = sink.start();

        // Summarization requests are recognizable by their system prompt.
        if context.system_prompt.as_deref() == Some(SUMMARIZATION_SYSTEM_PROMPT) {
            let message = assistant(
                model,
                vec![AssistantContent::Text(TextContent::plain(
                    "## Goal\nSummarized history.",
                ))],
            );
            let _ = sink.done(StopReason::Stop, message);
            return stream;
        }

        // A turn request: record what the loop actually sent.
        let first_text = context
            .messages
            .first()
            .and_then(|m| match m {
                Message::User(user) => match &user.content {
                    UserContentBody::Text(text) => Some(text.clone()),
                    UserContentBody::Blocks(_) => None,
                },
                _ => None,
            })
            .unwrap_or_default();
        self.seen_turn_requests
            .lock()
            .expect("test mutex")
            .push((first_text, context.messages.len()));

        let call = self.turn_calls.fetch_add(1, Ordering::SeqCst);
        if call < self.fail_first_turns {
            let message = AssistantMessage {
                stop_reason: StopReason::Error,
                error_message: Some(self.turn_error.to_string()),
                ..assistant(model, Vec::new())
            };
            let _ = sink.error(StopReason::Error, message);
        } else {
            let message = assistant(
                model,
                vec![AssistantContent::Text(TextContent::plain("done"))],
            );
            let _ = sink.done(StopReason::Stop, message);
        }
        stream
    }
}

fn mock_model(context_window: u64) -> Model {
    Model {
        id: "mock-model".into(),
        name: "Mock".into(),
        api: Api::from("mock"),
        provider: cupel_core::types::Provider::from("mock"),
        base_url: String::new(),
        reasoning: false,
        thinking_level_map: None,
        input: vec![cupel_core::types::InputModality::Text],
        cost: ModelCost {
            input: 0.0,
            output: 0.0,
            cached_read: 0.0,
            cached_write: 0.0,
        },
        context_window,
        max_tokens: 4096,
        headers: None,
        compat: None,
    }
}

async fn run_with(
    provider: Arc<CompactionAwareProvider>,
    context_window: u64,
    compaction: CompactionConfig,
    old_messages: Vec<AgentMessage>,
) -> Vec<AgentEvent> {
    let mut registry = Registry::new();
    registry.register(provider);

    let config = AgentLoopConfig {
        model: mock_model(context_window),
        thinking_level: None,
        api_key: Some("test".into()),
        session_id: None,
        temperature: None,
        max_tokens: None,
        tool_execution: ToolExecutionMode::Parallel,
        retry: RetryConfig {
            max_retries: 0, // isolate compaction from transient retry
            base_delay_ms: 1,
        },
        compaction,
    };
    let context = AgentContext {
        system_prompt: String::new(),
        messages: old_messages,
        tools: Vec::new(),
    };

    let (mut events, sink) = agent_event_channel();
    let loop_task = tokio::spawn(agent_loop(
        vec![AgentMessage::user_text("current question")],
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

fn compaction_events(events: &[AgentEvent]) -> Vec<(CompactionReason, bool)> {
    let mut out = Vec::new();
    let mut pending: Option<CompactionReason> = None;
    for event in events {
        match event {
            AgentEvent::CompactionStart { reason } => pending = Some(*reason),
            AgentEvent::CompactionEnd { error, .. } => {
                if let Some(reason) = pending.take() {
                    out.push((reason, error.is_none()));
                }
            }
            _ => {}
        }
    }
    out
}

/// ~1000 estimated tokens of filler per message (4000 chars / 4).
fn big_history(count: usize) -> Vec<AgentMessage> {
    (0..count)
        .map(|i| AgentMessage::user_text(format!("message {i}: {}", "x".repeat(4000))))
        .collect()
}

#[tokio::test]
async fn threshold_compaction_shrinks_the_request() {
    let provider = Arc::new(CompactionAwareProvider::new(0, ""));
    // Window 3000, reserve 1000 -> compaction at ~2000 estimated tokens.
    // Five ~1000-token messages of history is well past that.
    let config = CompactionConfig {
        enabled: true,
        reserve_tokens: 1000,
        keep_recent_tokens: 500,
    };
    let events = run_with(Arc::clone(&provider), 3000, config, big_history(5)).await;

    assert_eq!(
        compaction_events(&events),
        vec![(CompactionReason::Threshold, true)]
    );
    // Exactly one turn request, and it was the compacted one: it starts with
    // the summary marker and carries far fewer messages than 5 + prompt.
    let seen = provider.seen_turn_requests.lock().expect("test mutex");
    assert_eq!(seen.len(), 1);
    let (first_text, message_count) = &seen[0];
    assert!(
        first_text.starts_with(COMPACTION_MARKER),
        "request should start with the summary, got: {first_text:.60}"
    );
    assert!(
        *message_count <= 3,
        "5 history messages should have collapsed, got {message_count}"
    );
    assert!(matches!(events.last(), Some(AgentEvent::AgentEnd { .. })));
}

#[tokio::test]
async fn overflow_error_triggers_reactive_compaction_and_recovery() {
    // The provider rejects the FIRST turn request the way Anthropic does.
    let provider = Arc::new(CompactionAwareProvider::new(
        1,
        "prompt is too long: 250000 tokens > 200000 maximum",
    ));
    // Threshold generous enough that proactive compaction does NOT fire
    // (window 100k), so only the reactive path runs.
    let config = CompactionConfig {
        enabled: true,
        reserve_tokens: 1000,
        keep_recent_tokens: 500,
    };
    let events = run_with(Arc::clone(&provider), 100_000, config, big_history(4)).await;

    assert_eq!(
        compaction_events(&events),
        vec![(CompactionReason::Overflow, true)]
    );
    // Turn 1 failed with overflow, turn 2 (compacted) succeeded.
    assert_eq!(provider.turn_calls.load(Ordering::SeqCst), 2);
    let seen = provider.seen_turn_requests.lock().expect("test mutex");
    assert!(seen[1].0.starts_with(COMPACTION_MARKER));
    // The run recovered: last assistant message is a success.
    let recovered = events.iter().rev().find_map(|e| match e {
        AgentEvent::MessageEnd {
            message: AgentMessage::Llm(Message::Assistant(a)),
        } => Some(a.stop_reason),
        _ => None,
    });
    assert_eq!(recovered, Some(StopReason::Stop));
}

#[tokio::test]
async fn compaction_disabled_never_compacts() {
    let provider = Arc::new(CompactionAwareProvider::new(0, ""));
    let config = CompactionConfig {
        enabled: false,
        ..CompactionConfig::default()
    };
    let events = run_with(Arc::clone(&provider), 3000, config, big_history(5)).await;
    assert!(compaction_events(&events).is_empty());
}
