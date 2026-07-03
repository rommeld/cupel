//! The unified, provider-agnostic data model.
//!
//! Every provider speaks its own wire format, but the *rest of the application* only ever sees types.
//! Providers translate inbound/outbound.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use tokio_util::sync::CancellationToken;

// The wire protocol a model speaks (e.g. `"anthropic-message"). This is the
// key the provider registry dispatches on.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Api(pub String);

impl Api {
    // These strings must match pi's API identifiers exactly so persisted
    // sessions stay interchangeable between the two implementations.
    pub const ANTHROPIC_MESSAGES: &'static str = "anthropic-messages";
    pub const OPENAI_RESPONSES: &'static str = "openai-responses";
    pub const BEDROCK_CONVERSE_STREAM: &'static str = "bedrock-converse-stream";
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for Api {
    fn from(s: &str) -> Self {
        Api(s.to_string())
    }
}

impl core::fmt::Display for Api {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The concrete vendor/gateway (e.g. `"anthropic"`, `"openrouter"`).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Provider(pub String);

impl Provider {
    pub const ANTHROPIC: &'static str = "anthropic";
    pub const OPENAI: &'static str = "openai";
    pub const AMAZON_BEDROCK: &'static str = "amazon-bedrock";
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<&str> for Provider {
    fn from(s: &str) -> Self {
        Provider(s.to_string())
    }
}

/// A run of assistant/user text. `text_signature` is opaque provider metadata
/// (e.g. an `OpenAI` Responses message id) preserved for multi-turn replay.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TextContent {
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub text_signature: Option<String>,
}

impl TextContent {
    /// Plain text without provider metadata - the overwhelmingly common case.
    #[must_use]
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            text_signature: None,
        }
    }
}

/// Extended-thinking content. `thinking_signature` is the cryptographic
/// signature some providers require to *replay* a thinking block on the next
/// turn. `redacted` marks thinking the provider hid behind a safety filter -
/// the encrypted blob still rides along in the signature for continuity.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThinkingContent {
    pub thinking: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub thinking_signature: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub redacted: Option<bool>,
}

/// A base64-encoded inline image (no URLs, images always bytes).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageContent {
    pub data: String,
    pub mime_type: String,
}

/// A model's request to call a tool. `arguments` is free-form JSON; it's
/// expected to be a JSON object. During streaming it is only fully
/// populated once the tool call finishes. `thought_signature` is
/// Google-specific opaque thought context.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Value,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub thought_signature: Option<String>,
}

/// Allow content in a **user** message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum UserContent {
    Text(TextContent),
    Image(ImageContent),
}

/// Allow content in an **assistant** message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum AssistantContent {
    Text(TextContent),
    Thinking(ThinkingContent),
    ToolCall(ToolCall),
}

/// Allow content in a **tool result** message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ToolResultContent {
    Text(TextContent),
    Image(ImageContent),
}

// Create single text block with an `#[serde(untagged)]` enum so
// the JSON stays compatible, and provide constructors so callers
// rarely touch it directly.

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum UserContentBody {
    Text(String),
    Blocks(Vec<UserContent>),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserMessage {
    pub content: UserContentBody,
    /// Unix timestamp in milliseconds.
    pub timestamp: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AssistantMessage {
    pub content: Vec<AssistantContent>,
    pub api: Api,
    pub provider: Provider,
    pub model: String,
    /// Set when the upstream served a *different* concrete model than requested.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub response_model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub response_id: Option<String>,
    pub usage: Usage,
    pub stop_reason: StopReason,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub error_message: Option<String>,
    pub timestamp: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResultMessage {
    pub tool_call_id: String,
    pub tool_name: String,
    pub content: Vec<ToolResultContent>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub details: Option<Value>,
    pub is_error: bool,
    pub timestamp: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "camelCase")]
pub enum Message {
    User(UserMessage),
    Assistant(AssistantMessage),
    ToolResult(ToolResultMessage),
}

/// A tool the model may call. `parameters` is a JSON schema describing the
/// arguments.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tool {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

/// Everything needed to make a request, independent of which model handles it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Context {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub system_prompt: Option<String>,
    pub messages: Vec<Message>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub tools: Option<Vec<Tool>>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Cost {
    pub input: f64,
    pub output: f64,
    pub cache_read: f64,
    pub cache_write: f64,
    pub total: f64,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    /// Subset of `cache_write` written with 1h retention. Anthropic-only; it
    /// changes the cost formula (1h writes cost 2x base input).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub cache_write1h: Option<u64>,
    /// Reasoning/thinking tokens, when the provider reports them separately.
    /// They are a *subset* of `output`, not an addition to it.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub reasoning: Option<u64>,
    pub total_tokens: u64,
    pub cost: Cost,
}

/// `Error`/`Aborted` are terminal states (the provider maps vendor-specific
/// reasons onto these).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum StopReason {
    Stop,
    Length,
    ToolUse,
    Error,
    Aborted,
}

// Map provider specific reasoning scale on *unified* approach.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ThinkingLevel {
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
}

/// A thinking level *including* the "off" state, used by model metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ModelThinkingLevel {
    Off,
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
}

impl ModelThinkingLevel {
    /// String key used to look the level up in a model's `thinking_level_map`.
    #[must_use]
    pub fn as_str(&self) -> &'static str {
        match self {
            ModelThinkingLevel::Off => "off",
            ModelThinkingLevel::Minimal => "minimal",
            ModelThinkingLevel::Low => "low",
            ModelThinkingLevel::Medium => "medium",
            ModelThinkingLevel::High => "high",
            ModelThinkingLevel::XHigh => "xhigh",
        }
    }
}

pub type ThinkingLevelMap = BTreeMap<String, Option<String>>;

/// Per-level thinking token budgets for budget-based thinking models.
/// `None` fields fall back to the built-in defaults (1024/2048/8192/16384).
/// There is no `xhigh` budget: budget-based models clamp `xhigh` to `high`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThinkingBudgets {
    pub minimal: Option<u64>,
    pub low: Option<u64>,
    pub medium: Option<u64>,
    pub high: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CacheRetention {
    None,
    Short,
    Long,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Transport {
    Sse,
    Websocket,
    WebsocketCached,
    Auto,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum InputModality {
    Text,
    Image,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelCost {
    pub input: f64,
    pub output: f64,
    pub cached_read: f64,
    pub cached_write: f64,
}

/// Model descriptor.
/// TODO: translate into idiomatic Rust.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Model {
    pub id: String,
    pub name: String,
    pub api: Api,
    pub provider: Provider,
    /// API root, e.g. `https://api.anthropic.com`. Providers append their path.
    pub base_url: String,
    pub reasoning: bool,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub thinking_level_map: Option<ThinkingLevelMap>,
    pub input: Vec<InputModality>,
    pub cost: ModelCost,
    pub context_window: u64,
    pub max_tokens: u64,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub headers: Option<BTreeMap<String, String>>,
    /// API-specific compatibility overrides (raw JSON).
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub compat: Option<Value>,
}

// `StreamOptions` is *runtime configuration*, never serialized, so it carries no
// serde derives.
#[derive(Clone, Debug, Default)]
pub struct StreamOptions {
    pub temperature: Option<f64>,
    pub max_tokens: Option<u64>,
    pub api_key: Option<String>,
    /// Cooperative cancellation. Providers race their work against this token.
    pub signal: Option<CancellationToken>,
    pub transport: Option<Transport>,
    pub cache_retention: Option<CacheRetention>,
    pub session_id: Option<String>,
    pub headers: Option<BTreeMap<String, String>>,
    pub timeout_ms: Option<u64>,
    pub max_retries: Option<u32>,
    pub max_retry_delay_ms: Option<u64>,
    pub metadata: Option<BTreeMap<String, serde_json::Value>>,
    pub env: Option<BTreeMap<String, String>>,
    /// Unified reasoning level; the provider maps it to its own thinking config.
    pub reasoning: Option<ThinkingLevel>,
    /// Custom per-level thinking budgets (budget-based thinking models only).
    pub thinking_budgets: Option<ThinkingBudgets>,
}

/// Current Unix time in milliseconds. Messages carry creation timestamps
/// (pi uses `Date.now()`); this is the Rust equivalent.
#[must_use]
pub fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum AssistantMessageEvent {
    #[serde(rename = "start")]
    Start,

    #[serde(rename = "text_start", rename_all = "camelCase")]
    TextStart { content_index: usize },
    #[serde(rename = "text_delta", rename_all = "camelCase")]
    TextDelta { content_index: usize, delta: String },
    #[serde(rename = "text_end", rename_all = "camelCase")]
    TextEnd {
        content_index: usize,
        content: String,
    },

    #[serde(rename = "thinking_start", rename_all = "camelCase")]
    ThinkingStart { content_index: usize },
    #[serde(rename = "thinking_delta", rename_all = "camelCase")]
    ThinkingDelta { content_index: usize, delta: String },
    #[serde(rename = "thinking_end", rename_all = "camelCase")]
    ThinkingEnd {
        content_index: usize,
        content: String,
    },

    #[serde(rename = "toolcall_start", rename_all = "camelCase")]
    ToolCallStart { content_index: usize },
    #[serde(rename = "toolcall_delta", rename_all = "camelCase")]
    ToolCallDelta { content_index: usize, delta: String },
    #[serde(rename = "toolcall_end", rename_all = "camelCase")]
    ToolCallEnd {
        content_index: usize,
        tool_call: ToolCall,
    },

    #[serde(rename = "done")]
    Done {
        reason: StopReason,
        message: AssistantMessage,
    },
    #[serde(rename = "error")]
    Error {
        reason: StopReason,
        error: AssistantMessage,
    },
}
