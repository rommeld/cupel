use crate::{tool::CompletedToolCall, usage::Usage};

#[derive(Debug, Clone)]
pub enum InferenceEvent {
    TextDelta {
        text: String,
    },
    ThinkingDelta {
        text: String,
    },
    ToolCallStart {
        tool_call_start_index: usize,
        tool_call_start_id: String,
        tool_call_start_name: String,
    },
    ToolCallDelta {
        tool_call_delta_index: usize,
        partial_json: String,
    },
    ToolCallDone {
        tool_call_done_index: usize,
        call: CompletedToolCall,
    },
    Usage {
        usage: Usage,
    },
    Stop {
        reason: StopReason,
    },
}

#[derive(Debug, Clone)]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    ToolUse,
    Refusal,
    Unknown(String),
}
