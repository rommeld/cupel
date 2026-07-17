//! End-to-end proof that the bash denylist actually stops execution: a
//! scripted mock provider asks for `rm -rf /`, and the assertion is on
//! what flows back through the REAL agent loop - a blocked error
//! tool-result - not on the guard in isolation (guard.rs unit tests cover
//! that). Pattern copied from cupel-agent/tests/retry.rs.

#![allow(clippy::tests_outside_test_module)]

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use futures_util::StreamExt as _;

use cupel_agent::{Agent, AgentEvent, AgentMessage, AgentOptions, types::AgentTool};
use cupel_coding_agent::guard::BashGuard;
use cupel_coding_agent::tools::bash::BashTool;
use cupel_core::{
    event_stream::{AssistantMessageStream, assistant_message_channel},
    provider::{Provider, Registry},
    types::{
        Api, AssistantContent, AssistantMessage, Context, InputModality, Message, Model, ModelCost,
        StopReason, StreamOptions, TextContent, ToolCall, Usage, now_ms,
    },
};

/// Turn 1: request the forbidden bash command. Turn 2: acknowledge and
/// stop. The guard must intercept between the two.
struct DeleteHappyProvider {
    calls: AtomicU32,
}

impl Provider for DeleteHappyProvider {
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
        if call == 0 {
            let message = AssistantMessage {
                content: vec![AssistantContent::ToolCall(ToolCall {
                    id: "call_rm".into(),
                    name: "bash".into(),
                    arguments: serde_json::json!({"command": "rm -rf /"}),
                    thought_signature: None,
                })],
                stop_reason: StopReason::ToolUse,
                ..base
            };
            let _ = sink.done(StopReason::ToolUse, message);
        } else {
            let message = AssistantMessage {
                content: vec![AssistantContent::Text(TextContent::plain("understood"))],
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
async fn denied_bash_command_never_executes_and_the_model_learns_why() {
    let cwd = std::env::temp_dir().join("cupel-guard-veto-e2e");
    let _ = std::fs::remove_dir_all(&cwd);
    std::fs::create_dir_all(&cwd).unwrap();
    // A canary file the forbidden command would delete if it ever ran.
    std::fs::write(cwd.join("canary.txt"), "still here").unwrap();

    let mut registry = Registry::new();
    registry.register(Arc::new(DeleteHappyProvider {
        calls: AtomicU32::new(0),
    }));

    let mut options = AgentOptions::new(mock_model(), Arc::new(registry));
    options.api_key = Some("test".into());
    options.tools = vec![Arc::new(BashTool::new(&cwd)) as Arc<dyn AgentTool>];
    // No config files: the built-in defaults alone must block rm -rf.
    options.hooks = Arc::new(BashGuard::from_config(None, &cwd));
    let mut agent = Agent::new(options);

    let mut events = agent.prompt_text("delete everything").unwrap();
    let mut tool_results: Vec<String> = Vec::new();
    while let Some(event) = events.next().await {
        if let AgentEvent::MessageEnd {
            message: AgentMessage::Llm(Message::ToolResult(result)),
        } = event
        {
            tool_results.push(format!("{result:?}"));
        }
    }
    agent.wait_for_idle().await;

    // The loop turned the veto into an error tool-result naming the rule...
    assert_eq!(tool_results.len(), 1, "exactly one (blocked) tool result");
    assert!(
        tool_results[0].contains("denylist"),
        "the model sees why: {}",
        tool_results[0]
    );
    // ...and the command truly never ran.
    assert_eq!(
        std::fs::read_to_string(cwd.join("canary.txt")).unwrap(),
        "still here",
        "the canary survives"
    );
}
