//! End-to-end proof that the loop killer cuts off a repeating tool call:
//! a scripted mock provider requests the SAME bash append five times;
//! with maxRepeats = 2 the counter file gains exactly two lines, and
//! attempts three to five come back through the REAL agent loop as
//! blocked error tool-results. Pattern copied from tests/guard_veto.rs.

#![allow(clippy::tests_outside_test_module)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use futures_util::StreamExt as _;

use cupel_agent::{Agent, AgentEvent, AgentOptions, types::AgentTool};
use cupel_coding_agent::bootstrap::SessionHooks;
use cupel_coding_agent::guard::BashGuard;
use cupel_coding_agent::loop_killer::LoopKiller;
use cupel_coding_agent::tools::bash::BashTool;
use cupel_core::{
    event_stream::{AssistantMessageStream, assistant_message_channel},
    provider::{Provider, Registry},
    types::{
        Api, AssistantContent, AssistantMessage, Context, InputModality, Model, ModelCost,
        StopReason, StreamOptions, TextContent, ToolCall, ToolResultContent, Usage, now_ms,
    },
};

/// Five turns of the IDENTICAL append command, then a closing text turn.
struct StuckProvider {
    calls: AtomicU32,
}

impl Provider for StuckProvider {
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
        if call < 5 {
            let message = AssistantMessage {
                content: vec![AssistantContent::ToolCall(ToolCall {
                    // Unique id per attempt, IDENTICAL name + arguments -
                    // the killer must key on the call, never on the id.
                    id: format!("call_{call}"),
                    name: "bash".into(),
                    arguments: serde_json::json!({"command": "echo tick >> counter.txt"}),
                })],
                stop_reason: StopReason::ToolUse,
                ..base
            };
            let _ = sink.done(StopReason::ToolUse, message);
        } else {
            let message = AssistantMessage {
                content: vec![AssistantContent::Text(TextContent::plain("giving up"))],
                ..base
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
        input: vec![InputModality::Text],
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

#[tokio::test]
async fn repeated_identical_calls_are_cut_off_and_redirected() {
    let cwd = std::env::temp_dir().join("cupel-loop-killer-e2e");
    let _ = std::fs::remove_dir_all(&cwd);
    std::fs::create_dir_all(&cwd).unwrap();

    let mut registry = Registry::new();
    registry.register(Arc::new(StuckProvider {
        calls: AtomicU32::new(0),
    }));

    let mut options = AgentOptions::new(mock_model(), Arc::new(registry));
    options.api_key = Some("test".into());
    options.tools = vec![Arc::new(BashTool::new(&cwd)) as Arc<dyn AgentTool>];
    options.hooks = Arc::new(SessionHooks::new(
        BashGuard::from_config(None, &cwd),
        LoopKiller::new(Some(2)),
    ));
    let mut agent = Agent::new(options);

    let mut events = agent.prompt_text("keep counting").unwrap();
    let mut executions = 0_u32;
    let mut blocks: Vec<String> = Vec::new();
    while let Some(event) = events.next().await {
        if let AgentEvent::ToolExecutionEnd {
            result, is_error, ..
        } = event
        {
            let text: String = result
                .content
                .iter()
                .filter_map(|c| match c {
                    ToolResultContent::Text(t) => Some(t.text.as_str()),
                    ToolResultContent::Image(_) => None,
                })
                .collect::<Vec<_>>()
                .join("\n");
            if is_error {
                blocks.push(text);
            } else {
                executions += 1;
            }
        }
    }
    agent.wait_for_idle().await;

    // Physical proof: the command really ran exactly maxRepeats times.
    let counter = std::fs::read_to_string(cwd.join("counter.txt")).unwrap();
    assert_eq!(counter.lines().count(), 2, "counter.txt:\n{counter}");
    assert_eq!(executions, 2);
    assert_eq!(blocks.len(), 3, "attempts 3-5 must be blocked: {blocks:?}");
    assert!(
        blocks.iter().all(|b| b.contains("Loop killer")),
        "{blocks:?}"
    );
}
