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

use crate::{context::AssistantMessage, error::InferenceError};

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

/// A completed provider-neutral tool call.
///
/// This is the shape the runtime will eventually execute. The inference layer
/// only builds this value; it must not execute the tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    /// Provider tool-call ID if the provider gave one.
    ///
    /// Some providers identify calls by ID, some by index, and som by both.
    pub id: Option<String>,

    /// Stable call index within the assistant message.
    ///
    /// The index lets cupel merge deltas even when the provider sends the ID
    /// only after the first chunk.
    pub index: usize,

    /// Tool/function name selected by the model.
    pub name: String,

    ///  Parsed JSON arguments.
    ///
    /// The runtime can validate this against the tool schema before execution.
    pub arguments: Value,

    /// Raw argument JSON exactly as streamed.
    ///
    /// Keep this even when parsing succeeds; it is useful for debugging prodiver
    /// behavior and for showing the user what the model acutally emitted.
    pub raw_arguments: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallDelta {
    pub id: Option<String>,
    pub index: usize,
    pub name: Option<String>,

    /// Partial or complete JSON argument string.
    ///
    /// Some providers stream invalid/incomplete JSON fragments.
    pub arguments_delta: Option<String>,
}

/// In-progress tool call while streaming.
///
/// Provider deltas may arrive in several pieces. This struct keeps the partial
/// state until the provider says the call is done or assistant message ends.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AccumulatedToolCall {
    pub id: Option<String>,
    pub index: usize,
    pub name: Option<String>,
    pub raw_arguments: String,
}

impl AccumulatedToolCall {
    /// Apply one provider-neutral delta to this in-progress call.
    pub fn apply_delta(&mut self, delta: &ToolCallDelta) {
        // Providers sometimes repeat ID/name in later chunks. Keep the newest
        // non-empty values because they are usually more complete.
        if delta.id.is_some() {
            self.id = delta.id.clone();
        }

        if delta.name.is_some() {
            self.name = delta.name.clone();
        }

        if let Some(arguments_delta) = &delta.arguments_delta {
            self.raw_arguments.push_str(arguments_delta);
        }
    }

    /// Try to convert the accumulated state into a completed tool call.
    ///
    /// This returns a JSON parse error if the provider emitted malformed or
    /// incomplete JSON. The caller should keep `raw_arguments` so the model can
    /// be told exactly what went wrong.
    pub fn try_finish(&self) -> Result<ToolCall, serde_json::Error> {
        let arguments = serde_json::from_str::<Value>(&self.raw_arguments)?;

        Ok(ToolCall {
            id: self.id.clone(),
            index: self.index,
            name: self.name.clone().unwrap_or_default(),
            arguments,
            raw_arguments: self.raw_arguments.clone(),
        })
    }
}

/// Accumulates all streamed tool calls on one assistant message.
///
/// Deltas are merged by index because every provider-neutral `ToolCallDelta`
/// has an index. IDs are still preserved for later tool-result messages.
#[derive(Debug, Default)]
pub struct ToolCallAccumulator {
    calls: Vec<AccumulatedToolCall>,
}

impl ToolCallAccumulator {
    /// Add one streamed delta to the matching in-progress call.
    pub fn push_delta(&mut self, delta: ToolCallDelta) {
        let call = self.call_mut(delta.index);
        call.apply_delta(&delta);
    }

    /// Replace the accumulated raw argument string for one call.
    ///
    /// Some providers send a final authoritative argument string after streaming
    /// partial deltas. When the final string extends the current buffer, this
    /// returns the suffix that can still be emitted as a normal delta. If it
    /// does not extend the current buffer, the stored value is still replaced,
    /// but `None` is returned because provider-neutral deltas have no replace
    /// semantic.
    pub fn replace_arguments(&mut self, index: usize, arguments: String) -> Option<String> {
        let call = self.call_mut(index);
        let suffix = arguments
            .strip_prefix(&call.raw_arguments)
            .map(ToOwned::to_owned);
        call.raw_arguments = arguments;
        suffix
    }

    /// Return completed calls when JSON currently parses.
    ///
    /// This is useful at message end. Do not silently drop parse failures in
    /// provider code; tests should cover malformed JSON behavior explicitly.
    pub fn finish_all(&self) -> Vec<Result<ToolCall, serde_json::Error>> {
        self.calls
            .iter()
            .map(AccumulatedToolCall::try_finish)
            .collect()
    }

    fn call_mut(&mut self, index: usize) -> &mut AccumulatedToolCall {
        if let Some(position) = self.calls.iter().position(|call| call.index == index) {
            return self
                .calls
                .get_mut(position)
                .expect("position came from iterating over tool calls");
        }

        self.calls.push(AccumulatedToolCall {
            index,
            ..AccumulatedToolCall::default()
        });

        self.calls.last_mut().expect("just pushed a tool call")
    }
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
