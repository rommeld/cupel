//! Agent-level types: messages, tools, events, hooks.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use cupel_core::types::{
    AssistantMessage, Message, Model, ThinkingLevel, ToolResultContent, ToolResultMessage,
};

/// A message in the agent transcript: either one the LLM understands, or an
/// app-defined message (UI notification, artifact, ...) that is filtered out
/// (or converted) before each LLM call by [`AgentHooks::convert_to_llm`].
// The Llm variant is ~300 bytes vs Custom's ~64; boxing it would shrink the
// enum but add indirection to the hot path (every transcript access). A few
// hundred transcript entries at 300B is noise.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AgentMessage {
    Llm(Message),
    Custom {
        kind: String,
        payload: Value,
        timestamp: u64,
    },
}

impl AgentMessage {
    #[must_use]
    pub fn user_text(text: impl Into<String>) -> Self {
        AgentMessage::Llm(Message::User(cupel_core::types::UserMessage {
            content: cupel_core::types::UserContentBody::Text(text.into()),
            timestamp: cupel_core::types::now_ms(),
        }))
    }
}

/// Errors from tool execution. Tools should *throw* (return `Err`) on
/// failure; the loop converts errors into error tool-results for the model.
pub type ToolError = Box<dyn std::error::Error + Send + Sync>;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentToolResult {
    pub content: Vec<ToolResultContent>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub details: Option<Value>,
    #[serde(skip_serializing_if = "core::ops::Not::not", default)]
    pub terminate: bool,
}

impl AgentToolResult {
    #[must_use]
    pub fn text(text: impl Into<String>) -> Self {
        Self {
            content: vec![ToolResultContent::Text(
                cupel_core::types::TextContent::plain(text),
            )],
            details: None,
            terminate: false,
        }
    }
}

/// Callback tools use to stream partial results (progress) while executing.
pub type ToolUpdateFn = Arc<dyn Fn(AgentToolResult) + Send + Sync>;

/// A tool the agent can execute.
///
/// `#[async_trait]` is needed because native `async fn` in traits does not
/// yet support dynamic dispatch, and tools live behind `Arc<dyn AgentTool>`.
#[async_trait::async_trait]
pub trait AgentTool: Send + Sync {
    fn name(&self) -> &str;
    fn label(&self) -> &str {
        self.name()
    }
    fn description(&self) -> &str;
    fn parameters(&self) -> Value;
    fn execution_mode(&self) -> Option<ToolExecutionMode> {
        None
    }
    fn describe_call(&self, _args: &Value) -> String {
        self.name().to_string()
    }
    async fn execute(
        &self,
        tool_call_id: &str,
        args: Value,
        cancel: CancellationToken,
        on_update: Option<ToolUpdateFn>,
    ) -> Result<AgentToolResult, ToolError>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolExecutionMode {
    Sequential,
    #[default]
    Parallel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum QueueMode {
    All,
    #[default]
    OneAtATime,
}

/// Returned from [`AgentHooks::before_tool_call`] to veto a tool execution.
#[derive(Debug, Clone, Default)]
pub struct BeforeToolCallResult {
    pub block: bool,
    pub reason: Option<String>,
}

/// Context snapshot the low-level loop works on.
#[derive(Clone)]
pub struct AgentContext {
    pub system_prompt: String,
    pub messages: Vec<AgentMessage>,
    pub tools: Vec<Arc<dyn AgentTool>>,
}

/// Automatic-retry policy for transient provider failures.
///
/// Which errors count as transient is decided by
/// [`cupel_core::retry::is_retryable_assistant_error`]; this struct only
/// carries the budget. `max_retries: 0` disables retries entirely.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryConfig {
    pub max_retries: u32,
    /// First backoff delay; attempt N waits `base_delay_ms * 2^(N-1)`.
    pub base_delay_ms: u64,
}

impl Default for RetryConfig {
    fn default() -> Self {
        Self {
            max_retries: 3,
            base_delay_ms: 2000,
        }
    }
}

/// Everything the loop needs besides the context itself.
#[derive(Clone)]
pub struct AgentLoopConfig {
    pub model: Model,
    pub thinking_level: Option<ThinkingLevel>,
    pub api_key: Option<String>,
    pub session_id: Option<String>,
    pub temperature: Option<f64>,
    pub max_tokens: Option<u64>,
    pub tool_execution: ToolExecutionMode,
    pub retry: RetryConfig,
    pub compaction: crate::compaction::CompactionConfig,
}

/// Extension points for the agent loop. Every method has a sensible default,
/// so implementors override only what they need.
#[async_trait::async_trait]
pub trait AgentHooks: Send + Sync {
    /// Convert agent messages to LLM messages before each provider call.
    /// The default keeps LLM messages and drops custom ones.
    async fn convert_to_llm(&self, messages: &[AgentMessage]) -> Vec<Message> {
        messages
            .iter()
            .filter_map(|m| match m {
                AgentMessage::Llm(message) => Some(message.clone()),
                AgentMessage::Custom { .. } => None,
            })
            .collect()
    }

    /// Transform the transcript before `convert_to_llm` (pruning/compaction).
    async fn transform_context(&self, messages: Vec<AgentMessage>) -> Vec<AgentMessage> {
        messages
    }

    /// Resolve an API key for a provider right before each call. Useful for
    /// short-lived OAuth tokens that can expire during long tool phases.
    async fn api_key(&self, _provider: &str) -> Option<String> {
        None
    }

    /// Veto point before a tool executes.
    async fn before_tool_call(
        &self,
        _assistant: &AssistantMessage,
        _tool_call: &cupel_core::types::ToolCall,
    ) -> Option<BeforeToolCallResult> {
        None
    }

    /// Called after each turn; return `true` to stop the run gracefully.
    async fn should_stop_after_turn(
        &self,
        _message: &AssistantMessage,
        _tool_results: &[ToolResultMessage],
    ) -> bool {
        false
    }

    /// Messages to inject after the current turn ("steering").
    async fn steering_messages(&self) -> Vec<AgentMessage> {
        Vec::new()
    }

    /// Messages to process once the agent would otherwise stop.
    async fn follow_up_messages(&self) -> Vec<AgentMessage> {
        Vec::new()
    }
}

/// The no-hooks default.
pub struct NoHooks;

#[async_trait::async_trait]
impl AgentHooks for NoHooks {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionReason {
    Threshold,
    Overflow,
}

/// Events emitted by the agent for UIs. `AgentEnd` is always the last event
/// of a run.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    AgentEnd {
        messages: Vec<AgentMessage>,
    },
    TurnEnd {
        message: Box<AgentMessage>,
        tool_results: Vec<ToolResultMessage>,
    },
    MessageUpdate {
        event: cupel_core::types::AssistantMessageEvent,
    },
    MessageEnd {
        message: AgentMessage,
    },
    CompactionStart {
        reason: CompactionReason,
    },
    CompactionEnd {
        tokens_before: u64,
        tokens_after: u64,
        error: Option<String>,
        summary: Option<String>,
    },
    AutoRetry {
        attempt: u32,
        max_attempts: u32,
        delay_ms: u64,
        error_message: String,
    },
    ToolExecutionStart {
        tool_call_id: String,
        tool_name: String,
    },
    ToolExecutionUpdate {
        tool_call_id: String,
        tool_name: String,
        partial: AgentToolResult,
    },
    ToolExecutionEnd {
        tool_call_id: String,
        tool_name: String,
        result: AgentToolResult,
        is_error: bool,
    },
}
