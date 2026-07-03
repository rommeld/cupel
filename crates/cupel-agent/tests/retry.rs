//! End-to-end tests of the agent loop's automatic retry, using a scripted
//! mock provider. No network involved: the mock implements the same
//! [`Provider`] trait the real adapters do and fails on cue, which exercises
//! the exact code path a live 529 would take.

// Integration-test files under tests/ are compiled as their own crate and
// only ever built in test mode, so the "tests outside #[cfg(test)]"
// restriction lint does not apply here.
#![allow(clippy::tests_outside_test_module)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use futures_util::StreamExt as _;
use tokio_util::sync::CancellationToken;

use cupel_agent::{
    AgentEvent, AgentLoopConfig, AgentMessage, NoHooks, RetryConfig, ToolExecutionMode,
    agent_loop::{agent_event_channel, agent_loop},
    types::AgentContext,
};
use cupel_core::{
    event_stream::{AssistantMessageStream, assistant_message_channel},
    provider::{Provider, Registry},
    types::{
        Api, AssistantContent, AssistantMessage, Context, Model, ModelCost, StopReason,
        StreamOptions, TextContent, Usage, now_ms,
    },
};

/// A provider that fails its first `fail_times` calls with the given error
/// text, then succeeds with a plain text answer.
struct FlakyProvider {
    fail_times: u32,
    error_text: &'static str,
    calls: AtomicU32,
}

impl FlakyProvider {
    fn new(fail_times: u32, error_text: &'static str) -> Self {
        Self {
            fail_times,
            error_text,
            calls: AtomicU32::new(0),
        }
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
        timestamp: now_ms(),
    }
}

impl Provider for FlakyProvider {
    fn api(&self) -> &str {
        "mock"
    }

    fn stream(
        &self,
        model: &Model,
        _context: Context,
        _options: StreamOptions,
    ) -> AssistantMessageStream {
        let (stream, sink) = assistant_message_channel();
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let _ = sink.start();
        if call < self.fail_times {
            let message = AssistantMessage {
                stop_reason: StopReason::Error,
                error_message: Some(self.error_text.to_string()),
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
        reasoning: false,
        thinking_level_map: None,
        input: vec![cupel_core::types::InputModality::Text],
        cost: ModelCost {
            input: 0.0,
            output: 0.0,
            cached_read: 0.0,
            cached_write: 0.0,
        },
        context_window: 100_000,
        max_tokens: 4096,
        headers: None,
        compat: None,
    }
}

/// Run one prompt through the loop against `provider`, collecting all events.
async fn run_loop_with(provider: Arc<FlakyProvider>, retry: RetryConfig) -> Vec<AgentEvent> {
    let mut registry = Registry::new();
    registry.register(provider);

    let config = AgentLoopConfig {
        model: mock_model(),
        thinking_level: None,
        api_key: Some("test".into()),
        session_id: None,
        temperature: None,
        max_tokens: None,
        tool_execution: ToolExecutionMode::Parallel,
        retry,
        // Zero-size mock model window: compaction never interferes here.
        compaction: cupel_agent::CompactionConfig::default(),
    };
    let context = AgentContext {
        system_prompt: String::new(),
        messages: Vec::new(),
        tools: Vec::new(),
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

/// Fast retry policy so tests don't sleep for real.
fn fast_retry(max_retries: u32) -> RetryConfig {
    RetryConfig {
        max_retries,
        base_delay_ms: 1,
    }
}

fn auto_retries(events: &[AgentEvent]) -> Vec<(u32, u32)> {
    events
        .iter()
        .filter_map(|e| match e {
            AgentEvent::AutoRetry {
                attempt,
                max_attempts,
                ..
            } => Some((*attempt, *max_attempts)),
            _ => None,
        })
        .collect()
}

fn last_assistant(events: &[AgentEvent]) -> Option<AssistantMessage> {
    events.iter().rev().find_map(|e| match e {
        AgentEvent::MessageEnd {
            message: AgentMessage::Llm(cupel_core::types::Message::Assistant(a)),
        } => Some(a.clone()),
        _ => None,
    })
}

#[tokio::test]
async fn transient_failure_is_retried_and_recovers() {
    let provider = Arc::new(FlakyProvider::new(
        1,
        "provider returned HTTP 529: Overloaded",
    ));
    let events = run_loop_with(Arc::clone(&provider), fast_retry(3)).await;

    assert_eq!(auto_retries(&events), vec![(1, 3)]);
    assert_eq!(
        provider.calls.load(Ordering::SeqCst),
        2,
        "one failure + one retry"
    );
    let final_message = last_assistant(&events).expect("assistant message");
    assert_eq!(final_message.stop_reason, StopReason::Stop);
    // The run ended normally, not in the error path.
    assert!(matches!(events.last(), Some(AgentEvent::AgentEnd { .. })));
}

#[tokio::test]
async fn retry_budget_exhaustion_surfaces_the_error() {
    // Fails more times than the budget allows: 1 initial try + 2 retries.
    let provider = Arc::new(FlakyProvider::new(10, "503 Service Unavailable"));
    let events = run_loop_with(Arc::clone(&provider), fast_retry(2)).await;

    assert_eq!(auto_retries(&events), vec![(1, 2), (2, 2)]);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 3);
    let final_message = last_assistant(&events).expect("assistant message");
    assert_eq!(final_message.stop_reason, StopReason::Error);
}

#[tokio::test]
async fn non_retryable_errors_fail_immediately() {
    let provider = Arc::new(FlakyProvider::new(
        10,
        "insufficient_quota: check your plan and billing details",
    ));
    let events = run_loop_with(Arc::clone(&provider), fast_retry(3)).await;

    assert!(
        auto_retries(&events).is_empty(),
        "billing errors must not retry"
    );
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn retries_disabled_with_zero_budget() {
    let provider = Arc::new(FlakyProvider::new(10, "Overloaded"));
    let events = run_loop_with(Arc::clone(&provider), fast_retry(0)).await;

    assert!(auto_retries(&events).is_empty());
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
}
