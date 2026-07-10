//! Agent runtime for cupel: the loop that alternates between model responses
//! and tool executions, plus a stateful [`Agent`](agent::Agent) wrapper.
//!
//! Layering:
//! - [`agent_loop`] - the pure loop: context in, events + new messages out.
//!   No state of its own; testable in isolation.
//! - [`agent`] - owns a transcript, queues, and abort handling on top.
//! - [`types`] - messages, tools, hooks, events.

pub mod agent;
pub mod agent_loop;
pub mod compaction;
pub mod types;

pub use agent::{Agent, AgentError, AgentOptions, AgentState};
pub use agent_loop::{AgentEventStream, agent_loop, agent_loop_continue};
pub use compaction::CompactionConfig;
pub use types::{
    AgentContext, AgentEvent, AgentHooks, AgentLoopConfig, AgentMessage, AgentTool,
    AgentToolResult, CompactionReason, NoHooks, QueueMode, RetryConfig, ToolError,
    ToolExecutionMode,
};
