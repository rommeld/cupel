use serde_json::Value;

use crate::tool::ToolSpec;

#[derive(Debug, Clone)]
pub struct Inference {
    pub inference_messages: Vec<Message>,
    pub system_prompt: Option<String>,
    pub tools: Vec<ToolSpec>,
}

#[derive(Debug, Clone)]
pub struct Message {
    pub message_content: Vec<ContentBlock>,
    pub message_role: MessageRole,
}

#[derive(Debug, Clone)]
pub enum MessageRole {
    Assistant,
    Tool,
    User,
}

#[derive(Debug, Clone)]
pub enum ContentBlock {
    Text(String),
    ToolResult {
        tool_result_id: String,
        tool_result_content: String,
        tool_result_is_error: bool,
    },
    ToolUse {
        tool_use_id: String,
        tool_use_name: String,
        tool_use_input: Value,
    },
}
