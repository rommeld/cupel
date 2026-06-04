//! Tools should only be described to the model but not executed in the inference layer.
//!
//! Inference layer only provides tools to the agent layer, while model API only
//! serializes tool definitions and tool calls.
//! Basic tools are 'read', 'write', 'edit', and 'bash'.
use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ToolName(pub String);

/// Minimal JSON schema.
///
/// Keep generic. Do not bind `cupel-inference` to a specific schema crate.
/// The runtime/tool crates provide typed builders.
pub type JsonSchema = Value;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: ToolName,
    pub description: String,
    pub parameters: JsonSchema,

    // Provider-specific flags can be added without contaminating core fields.
    #[serde(default)]
    pub metadata: BTreeMap<String, Value>,
}
