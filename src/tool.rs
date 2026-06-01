use serde_json::Value;

#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub tool_name: String,
    pub tool_description: String,
    pub tool_input_schema: Value,
}

#[derive(Debug, Clone)]
pub struct CompletedToolCall {
    pub completed_tool_call_id: String,
    pub completed_tool_call_name: String,
    pub completed_tool_call_input: Value,
}
