//! Agent stream definition to preserve:
//!
//! - text deltas,
//! - reasoning/thinking deltas where available,
//! - tool-call start/delta/end semantics,
//! - usage and finish reason,
//! - errors,
//! - optional raw debugging payloads
use core::pin::Pin;

use futures::Stream;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{AssistantMessage, error::InferenceError};

pub type InferenceStream = Pin<Box<dyn Stream<Item = AssistantMessageEvent> + Send + 'static>>;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinishReason {
    Stop,
    Length,
    ToolCalls,
    ContentFilter,
    Error,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallDelta {
    pub id: Option<String>,
    pub index: usize,
    pub name: Option<String>,

    /// Partial or complete JSON argument string.
    ///
    /// Some providers stream invalid/incomplete JSON fragements.
    pub arguments_delta: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssistantMessageDelta {
    pub text_delta: Option<String>,
    pub thinking_delta: Option<String>,
    pub tool_call_delta: Option<ToolCallDelta>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AssistantMessageEvent {
    Start {
        message: AssistantMessage,
    },
    TextDelta {
        delta: String,
        message: AssistantMessage,
    },
    ThinkingDelta {
        delta: String,
        message: AssistantMessage,
    },
    ToolCallDelta {
        delta: ToolCallDelta,
        message: AssistantMessage,
    },
    Done {
        message: AssistantMessage,
    },
    Error {
        error: InferenceError,
        message: AssistantMessage,
    },
    RawProviderEvent {
        provider: String,
        payload: Value,
    },
}
