//! Agent-level types: messages, tools, events, hooks.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use cupel_core::types::{
    AssistantMessage, Message, Model, ThinkingLevel, ToolResultContent, ToolResultMessage,
};

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

/// A message in the agent transcript: either one the LLM understands, or an
/// app-defined message (UI notification, artifact, ...) that is filtered out
/// (or converted) before each LLM call by [`AgentHooks::convert_to_llm`].
// The Llm variant is ~300 bytes vs Custom's ~64; boxing it would shrink the
// enum but add indirection to the hot path (every transcript access). A few
// hundred transcript entries at 300B is noise - size is not the constraint.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AgentMessage {
    Llm(Message),
    Custom {
        /// App-defined discriminator, e.g. `"notification"`.
        kind: String,
        payload: Value,
        timestamp: u64,
    },
}

impl AgentMessage {
    /// Convenience: wrap a plain user text message.
    #[must_use]
    pub fn user_text(text: impl Into<String>) -> Self {
        AgentMessage::Llm(Message::User(cupel_core::types::UserMessage {
            content: cupel_core::types::UserContentBody::Text(text.into()),
            timestamp: cupel_core::types::now_ms(),
        }))
    }
}

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

/// Errors from tool execution. Tools should *throw* (return `Err`) on
/// failure; the loop converts errors into error tool-results for the model.
pub type ToolError = Box<dyn std::error::Error + Send + Sync>;

/// Final or partial result produced by a tool.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentToolResult {
    /// Text or image content returned to the model.
    pub content: Vec<ToolResultContent>,
    /// Arbitrary structured details for logs or UI rendering.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub details: Option<Value>,
    /// Hint that the agent should stop after the current tool batch.
    /// Early termination only happens when EVERY tool result in the batch
    /// sets this.
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
    /// Wire name the model calls the tool by.
    fn name(&self) -> &str;
    /// Human-readable label for UIs.
    fn label(&self) -> &str {
        self.name()
    }
    /// Description shown to the model.
    fn description(&self) -> &str;
    /// JSON Schema for the arguments object.
    fn parameters(&self) -> Value;
    /// Per-tool execution mode override. A single `Sequential` tool in a
    /// batch forces the whole batch to run sequentially.
    fn execution_mode(&self) -> Option<ToolExecutionMode> {
        None
    }

    /// Execute the tool. Deserialize `args` with serde (that is the schema
    /// validation), honor `cancel`, optionally push progress via `on_update`.
    async fn execute(
        &self,
        tool_call_id: &str,
        args: Value,
        cancel: CancellationToken,
        on_update: Option<ToolUpdateFn>,
    ) -> Result<AgentToolResult, ToolError>;
}

// ---------------------------------------------------------------------------
// Execution configuration
// ---------------------------------------------------------------------------

/// How tool calls from one assistant message are executed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolExecutionMode {
    /// Each call is prepared, executed, and finalized before the next starts.
    Sequential,
    /// Calls are prepared sequentially, then executed concurrently.
    /// `ToolExecutionEnd` fires in completion order; tool-result messages are
    /// emitted later in assistant source order. (pi's default too.)
    #[default]
    Parallel,
}

/// How many queued messages are injected at a queue drain point.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum QueueMode {
    /// Drain the whole queue at once.
    All,
    /// Inject only the oldest message, leaving the rest for later drains.
    #[default]
    OneAtATime,
}

/// Returned from [`AgentHooks::before_tool_call`] to veto a tool execution.
#[derive(Debug, Clone, Default)]
pub struct BeforeToolCallResult {
    pub block: bool,
    /// Text of the error tool-result emitted when blocked.
    pub reason: Option<String>,
}

/// Returned from [`AgentHooks::after_tool_call`] to override parts of an
/// executed tool result. `None` fields keep the executed values (field-by-
/// field merge, no deep merge - same as pi).
#[derive(Debug, Clone, Default)]
pub struct AfterToolCallResult {
    pub content: Option<Vec<ToolResultContent>>,
    pub details: Option<Value>,
    pub is_error: Option<bool>,
    pub terminate: Option<bool>,
}

/// Replacement state applied before the next provider request in a run.
#[derive(Debug, Clone, Default)]
pub struct AgentLoopTurnUpdate {
    pub model: Option<Model>,
    /// `Some(None)` switches thinking off; `None` keeps the current level.
    pub thinking_level: Option<Option<ThinkingLevel>>,
}

/// Context snapshot the low-level loop works on.
#[derive(Clone)]
pub struct AgentContext {
    pub system_prompt: String,
    pub messages: Vec<AgentMessage>,
    pub tools: Vec<Arc<dyn AgentTool>>,
}

/// Automatic-retry policy for transient provider failures (pi's session
/// retry settings: 3 retries, 2s base delay, exponential backoff).
///
/// Which errors count as transient is decided by
/// [`cupel_core::retry::is_retryable_assistant_error`]; this struct only
/// carries the budget. `max_retries: 0` disables retries entirely.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryConfig {
    /// Maximum CONSECUTIVE retries; a successful response resets the count.
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
    /// `None` = thinking off.
    pub thinking_level: Option<ThinkingLevel>,
    /// Fallback API key when [`AgentHooks::api_key`] returns `None`.
    pub api_key: Option<String>,
    /// Session id forwarded to providers for cache affinity.
    pub session_id: Option<String>,
    pub temperature: Option<f64>,
    pub max_tokens: Option<u64>,
    pub tool_execution: ToolExecutionMode,
    pub retry: RetryConfig,
    pub compaction: crate::compaction::CompactionConfig,
}

// ---------------------------------------------------------------------------
// Hooks
// ---------------------------------------------------------------------------

/// Extension points for the agent loop. Every method has a sensible default,
/// so implementors override only what they need.
///
/// Contract (same as pi): hook implementations must not panic; a panicking
/// hook tears down the loop without a normal event sequence.
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

    /// Override point after a tool executed, before events are emitted.
    async fn after_tool_call(
        &self,
        _assistant: &AssistantMessage,
        _tool_call: &cupel_core::types::ToolCall,
        _result: &AgentToolResult,
        _is_error: bool,
    ) -> Option<AfterToolCallResult> {
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

    /// Swap model/thinking level between turns of one run.
    async fn prepare_next_turn(&self) -> Option<AgentLoopTurnUpdate> {
        None
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

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// What triggered a compaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionReason {
    /// The pre-turn estimate crossed the reserve threshold.
    Threshold,
    /// The provider rejected a request as exceeding the context window.
    Overflow,
}

/// Events emitted by the agent for UIs. `AgentEnd` is always the last event
/// of a run.
#[derive(Debug, Clone)]
pub enum AgentEvent {
    // -- run lifecycle --
    AgentStart,
    AgentEnd {
        messages: Vec<AgentMessage>,
    },
    // -- turn lifecycle: one assistant response + its tool calls/results --
    TurnStart,
    TurnEnd {
        message: Box<AgentMessage>,
        tool_results: Vec<ToolResultMessage>,
    },
    // -- message lifecycle (user, assistant, and tool-result messages) --
    MessageStart {
        message: AgentMessage,
    },
    /// Streaming update for the in-flight assistant message. Carries the raw
    /// provider event; UIs that want the partial message apply the deltas.
    MessageUpdate {
        event: cupel_core::types::AssistantMessageEvent,
    },
    MessageEnd {
        message: AgentMessage,
    },
    /// Context compaction is starting: old history is being summarized to
    /// fit the context window. The loop pauses until it finishes.
    CompactionStart {
        reason: CompactionReason,
    },
    /// Compaction finished. `error: Some(..)` means it failed; the loop
    /// proceeds anyway (the next request may still fit - if not, the
    /// overflow error reaches the user normally).
    CompactionEnd {
        tokens_before: u64,
        tokens_after: u64,
        error: Option<String>,
    },
    /// A transient provider failure is about to be retried after a backoff
    /// wait. The errored turn already ended normally (`TurnEnd` fired); a
    /// fresh `TurnStart` follows once the wait elapses. pi additionally
    /// emits an `auto_retry_end` event; here success is observable from the
    /// next assistant message, so one event carries all the signal.
    AutoRetry {
        /// 1-based attempt number.
        attempt: u32,
        max_attempts: u32,
        delay_ms: u64,
        error_message: String,
    },
    // -- tool execution lifecycle --
    ToolExecutionStart {
        tool_call_id: String,
        tool_name: String,
        args: Value,
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
