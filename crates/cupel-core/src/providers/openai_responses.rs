//! `OpenAI` Responses API provider.
//!
//! Flow: build JSON request -> POST `{base_url}/responses` with
//! `stream: true` -> decode the SSE body -> translate Responses events into
//! unified [`AssistantMessageEvent`]s.
//!
//! The Responses API differs from Anthropic's in two ways that shape this
//! code:
//!
//! 1. **Item-based transcript.** Instead of role-tagged messages, requests
//!    and responses are flat lists of typed *items* (`message`, `reasoning`,
//!    `function_call`, `function_call_output`). Replaying a conversation
//!    means reconstructing those items, including their server-assigned ids.
//!    We smuggle the ids through our unified types: a text block keeps its
//!    item id in `text_signature`, a reasoning item is stored *whole* as JSON
//!    in `thinking_signature`, and a tool-call id is `"{call_id}|{item_id}"`.
//! 2. **Stateless encrypted reasoning.** With `store: false` the server
//!    returns reasoning as an encrypted blob (`reasoning.encrypted_content`)
//!    which must be replayed verbatim on the next turn.

use serde_json::{Value, json};

use crate::{
    error::{InferenceError, Result},
    event_stream::{AssistantMessageStream, EventSink},
    json_util::parse_streaming_json,
    model::calculate_cost,
    options_util::clamp_max_tokens_to_context,
    provider::Provider,
    providers::{
        CONTENT_FILTER_MESSAGE, EffortStyle, apply_custom_headers, compat, finish_output,
        new_output_message, off_effort, parse_compat, send_request, spawn_provider_stream,
        thinking_effort, with_cancel,
    },
    sse::{ServerSentEvent, SseDecoder},
    transform::transform_messages,
    types::{
        Api, AssistantContent, AssistantMessage, CacheRetention, Context, Message, Model,
        StopReason, StreamOptions, TextContent, ThinkingContent, ToolCall, ToolResultContent,
        UserContent, UserContentBody,
    },
};

pub struct OpenAiResponsesProvider {
    http: reqwest::Client,
}

impl OpenAiResponsesProvider {
    #[must_use]
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
        }
    }
}

impl Default for OpenAiResponsesProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl Provider for OpenAiResponsesProvider {
    fn api(&self) -> &str {
        Api::OPENAI_RESPONSES
    }

    fn stream(
        &self,
        model: &Model,
        context: Context,
        options: StreamOptions,
    ) -> AssistantMessageStream {
        let http = self.http.clone();
        spawn_provider_stream(model, move |model, sink| async move {
            run(&http, &model, &context, &options, &sink).await
        })
    }

    fn compat_problems(&self, model: &Model) -> Vec<String> {
        compat_problems(model)
    }
}

/// Encode `{"v":1,"id":...}`. Versioning the payload
/// lets future formats coexist with already-persisted sessions.
fn encode_text_signature(id: &str, phase: Option<&str>) -> String {
    let mut payload = json!({"v": 1, "id": id});
    if let Some(phase) = phase {
        payload["phase"] = json!(phase);
    }
    payload.to_string()
}

/// Parse a text signature, tolerating the legacy format where the signature
/// was the bare item id.
fn parse_text_signature(signature: &str) -> (String, Option<String>) {
    if signature.starts_with('{')
        && let Ok(parsed) = serde_json::from_str::<Value>(signature)
        && parsed.get("v").and_then(Value::as_u64) == Some(1)
        && let Some(id) = parsed.get("id").and_then(Value::as_str)
    {
        let phase = parsed
            .get("phase")
            .and_then(Value::as_str)
            .filter(|p| *p == "commentary" || *p == "final_answer")
            .map(str::to_string);
        return (id.to_string(), phase);
    }
    (signature.to_string(), None)
}

/// Cheap deterministic hash used to shorten foreign/oversized ids. FNV-1a is
/// tiny, dependency-free, and collision-resistant enough for id dedup.
pub(crate) fn short_hash(input: &str) -> String {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = FNV_OFFSET;
    for byte in input.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    format!("{hash:016x}")
}

pub(crate) fn normalize_id_part(part: &str) -> String {
    let sanitized: String = part
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect();
    sanitized.trim_end_matches('_').to_string()
}

/// Tool-call id normalization for replay. Our ids are `"{call_id}|{item_id}"`;
/// the item id must start with `fc_` or the API rejects it, and ids minted by
/// a *different* provider are replaced by a hash-derived one to avoid
/// colliding with OpenAI's pairing validation.
fn normalize_tool_call_id(id: &str, model: &Model, source: &AssistantMessage) -> String {
    let Some((call_id, item_id)) = id.split_once('|') else {
        return normalize_id_part(id);
    };
    let normalized_call_id = normalize_id_part(call_id);
    let is_foreign = source.provider != model.provider || source.api != model.api;
    let mut normalized_item_id = if is_foreign {
        format!("fc_{}", short_hash(item_id))
    } else {
        normalize_id_part(item_id)
    };
    if !normalized_item_id.starts_with("fc_") {
        normalized_item_id = normalize_id_part(&format!("fc_{normalized_item_id}"));
    }
    format!("{normalized_call_id}|{normalized_item_id}")
}

#[tracing::instrument(name = "openai_responses_request", skip_all, fields(model = %model.id, provider = %model.provider.as_str()))]
async fn run(
    http: &reqwest::Client,
    model: &Model,
    context: &Context,
    options: &StreamOptions,
    sink: &EventSink,
) -> Result<()> {
    let api_key = options
        .api_key
        .clone()
        .ok_or_else(|| InferenceError::MissingApiKey(model.provider.as_str().to_string()))?;

    let body = build_request_body(model, context, options);
    // TRACE only: request bodies contain the user's code and prompts.
    tracing::trace!(body = %body, "request body");
    let url = format!("{}/responses", model.base_url.trim_end_matches('/'));

    let mut req = http
        .post(&url)
        .header("authorization", format!("Bearer {api_key}"))
        .header("content-type", "application/json")
        .header("accept", "application/json");
    req = apply_custom_headers(req, model, options);
    if let Some(timeout) = options.timeout_ms {
        req = req.timeout(core::time::Duration::from_millis(timeout));
    }

    let response = send_request(req.json(&body), options).await?;

    process_response_stream(response, model, options, sink).await
}

/// Decode one Responses SSE body into unified events.
pub(crate) async fn process_response_stream(
    response: reqwest::Response,
    model: &Model,
    options: &StreamOptions,
    sink: &EventSink,
) -> Result<()> {
    let mut output = new_output_message(model);
    if !sink.start() {
        return Ok(());
    }

    // The Responses API addresses items by `output_index`. Each in-flight
    // item maps to a slot: our content index + the tool-call JSON scratch.
    struct Slot {
        content_index: usize,
        kind: SlotKind,
    }
    enum SlotKind {
        Thinking,
        Text,
        ToolCall { partial_json: String },
    }
    let mut slots: std::collections::HashMap<u64, Slot> = std::collections::HashMap::new();
    let mut saw_terminal_response = false;

    use futures_util::StreamExt as _;
    let mut byte_stream = response.bytes_stream();
    let mut decoder = SseDecoder::new();
    let mut events: Vec<ServerSentEvent> = Vec::new();

    // Creating a slot when `response.output_item.added` arrives; `.done`
    // events can also create one defensively.
    fn create_slot(
        slots: &mut std::collections::HashMap<u64, Slot>,
        output_index: u64,
        item: &Value,
        output: &mut AssistantMessage,
        sink: &EventSink,
    ) -> bool {
        let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");
        match item_type {
            "reasoning" => {
                output
                    .content
                    .push(AssistantContent::Thinking(ThinkingContent {
                        thinking: String::new(),
                        thinking_signature: None,
                        redacted: None,
                    }));
                let content_index = output.content.len() - 1;
                slots.insert(
                    output_index,
                    Slot {
                        content_index,
                        kind: SlotKind::Thinking,
                    },
                );
                sink.thinking_start(content_index)
            }
            "message" => {
                output
                    .content
                    .push(AssistantContent::Text(TextContent::plain("")));
                let content_index = output.content.len() - 1;
                slots.insert(
                    output_index,
                    Slot {
                        content_index,
                        kind: SlotKind::Text,
                    },
                );
                sink.text_start(content_index)
            }
            "function_call" => {
                let call_id = item.get("call_id").and_then(Value::as_str).unwrap_or("");
                let item_id = item.get("id").and_then(Value::as_str).unwrap_or("");
                let initial_args = item
                    .get("arguments")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string();
                output.content.push(AssistantContent::ToolCall(ToolCall {
                    // Both halves are needed for replay; see module docs.
                    id: format!("{call_id}|{item_id}"),
                    name: item
                        .get("name")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    arguments: json!({}),
                }));
                let content_index = output.content.len() - 1;
                slots.insert(
                    output_index,
                    Slot {
                        content_index,
                        kind: SlotKind::ToolCall {
                            partial_json: initial_args,
                        },
                    },
                );
                sink.toolcall_start(content_index)
            }
            // Other item types (web_search_call, ...) are not requested.
            _ => true,
        }
    }

    'outer: loop {
        let chunk = with_cancel(options, byte_stream.next()).await?;
        let done = chunk.is_none();
        match chunk {
            Some(chunk) => decoder.push(&chunk?, &mut events),
            None => decoder.finish(&mut events),
        }

        for sse in events.drain(..) {
            let Ok(data) = serde_json::from_str::<Value>(&sse.data) else {
                continue;
            };
            let event_type = data.get("type").and_then(Value::as_str).unwrap_or("");
            let output_index = data.get("output_index").and_then(Value::as_u64);

            match event_type {
                "response.created" => {
                    output.response_id = data
                        .get("response")
                        .and_then(|r| r.get("id"))
                        .and_then(Value::as_str)
                        .map(str::to_string);
                }

                "response.output_item.added" => {
                    if let (Some(index), Some(item)) = (output_index, data.get("item"))
                        && !create_slot(&mut slots, index, item, &mut output, sink)
                    {
                        break 'outer;
                    }
                }

                // Reasoning arrives either as summaries (o-series) or as raw
                // reasoning text; both append to the same thinking block.
                "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                    let delta = data.get("delta").and_then(Value::as_str).unwrap_or("");
                    if let Some(slot) = output_index.and_then(|i| slots.get(&i))
                        && matches!(slot.kind, SlotKind::Thinking)
                        && let Some(AssistantContent::Thinking(block)) =
                            output.content.get_mut(slot.content_index)
                    {
                        block.thinking.push_str(delta);
                        if !sink.thinking_delta(slot.content_index, delta.to_string()) {
                            break 'outer;
                        }
                    }
                }

                // Paragraph separator between summary parts.
                "response.reasoning_summary_part.done" => {
                    if let Some(slot) = output_index.and_then(|i| slots.get(&i))
                        && matches!(slot.kind, SlotKind::Thinking)
                        && let Some(AssistantContent::Thinking(block)) =
                            output.content.get_mut(slot.content_index)
                    {
                        block.thinking.push_str("\n\n");
                        if !sink.thinking_delta(slot.content_index, "\n\n".to_string()) {
                            break 'outer;
                        }
                    }
                }

                // Refusals stream like text and land in the same block.
                "response.output_text.delta" | "response.refusal.delta" => {
                    let delta = data.get("delta").and_then(Value::as_str).unwrap_or("");
                    if let Some(slot) = output_index.and_then(|i| slots.get(&i))
                        && matches!(slot.kind, SlotKind::Text)
                        && let Some(AssistantContent::Text(block)) =
                            output.content.get_mut(slot.content_index)
                    {
                        block.text.push_str(delta);
                        if !sink.text_delta(slot.content_index, delta.to_string()) {
                            break 'outer;
                        }
                    }
                }

                "response.function_call_arguments.delta" => {
                    let delta = data.get("delta").and_then(Value::as_str).unwrap_or("");
                    if let Some(slot) = output_index.and_then(|i| slots.get_mut(&i))
                        && let SlotKind::ToolCall { partial_json } = &mut slot.kind
                    {
                        partial_json.push_str(delta);
                        let parsed = parse_streaming_json(partial_json);
                        if let Some(AssistantContent::ToolCall(tc)) =
                            output.content.get_mut(slot.content_index)
                        {
                            tc.arguments = parsed;
                        }
                        if !sink.toolcall_delta(slot.content_index, delta.to_string()) {
                            break 'outer;
                        }
                    }
                }

                "response.function_call_arguments.done" => {
                    // The event carries the full argument string; emit only
                    // the suffix we haven't already streamed as a delta.
                    let full = data.get("arguments").and_then(Value::as_str).unwrap_or("");
                    if let Some(slot) = output_index.and_then(|i| slots.get_mut(&i))
                        && let SlotKind::ToolCall { partial_json } = &mut slot.kind
                    {
                        let suffix = full
                            .strip_prefix(partial_json.as_str())
                            .unwrap_or("")
                            .to_string();
                        *partial_json = full.to_string();
                        let parsed = parse_streaming_json(partial_json);
                        if let Some(AssistantContent::ToolCall(tc)) =
                            output.content.get_mut(slot.content_index)
                        {
                            tc.arguments = parsed;
                        }
                        if !suffix.is_empty() && !sink.toolcall_delta(slot.content_index, suffix) {
                            break 'outer;
                        }
                    }
                }

                "response.output_item.done" => {
                    let Some(index) = output_index else { continue };
                    let Some(item) = data.get("item") else {
                        continue;
                    };
                    // Defensive: create the slot if `.added` was never seen.
                    if !slots.contains_key(&index)
                        && !create_slot(&mut slots, index, item, &mut output, sink)
                    {
                        break 'outer;
                    }
                    let Some(slot) = slots.remove(&index) else {
                        continue;
                    };
                    let item_type = item.get("type").and_then(Value::as_str).unwrap_or("");

                    match (item_type, &slot.kind) {
                        ("reasoning", SlotKind::Thinking) => {
                            if let Some(AssistantContent::Thinking(block)) =
                                output.content.get_mut(slot.content_index)
                            {
                                // Prefer the item's final summary/content text.
                                let summary = join_texts(item.get("summary"), "text");
                                let content = join_texts(item.get("content"), "text");
                                if !summary.is_empty() {
                                    block.thinking = summary;
                                } else if !content.is_empty() {
                                    block.thinking = content;
                                }
                                // Store the entire reasoning item; replaying it
                                // verbatim is what keeps encrypted reasoning
                                // valid across turns.
                                block.thinking_signature = Some(item.to_string());
                                let text = block.thinking.clone();
                                if !sink.thinking_end(slot.content_index, text) {
                                    break 'outer;
                                }
                            }
                        }
                        ("message", SlotKind::Text) => {
                            if let Some(AssistantContent::Text(block)) =
                                output.content.get_mut(slot.content_index)
                            {
                                let full_text = item
                                    .get("content")
                                    .and_then(Value::as_array)
                                    .map(|parts| {
                                        parts
                                            .iter()
                                            .filter_map(|p| {
                                                p.get("text")
                                                    .or_else(|| p.get("refusal"))
                                                    .and_then(Value::as_str)
                                            })
                                            .collect::<Vec<_>>()
                                            .join("")
                                    })
                                    .unwrap_or_default();
                                if !full_text.is_empty() {
                                    block.text = full_text;
                                }
                                let id = item.get("id").and_then(Value::as_str).unwrap_or("");
                                let phase = item.get("phase").and_then(Value::as_str);
                                block.text_signature = Some(encode_text_signature(id, phase));
                                let text = block.text.clone();
                                if !sink.text_end(slot.content_index, text) {
                                    break 'outer;
                                }
                            }
                        }
                        ("function_call", SlotKind::ToolCall { partial_json }) => {
                            let args_str = item
                                .get("arguments")
                                .and_then(Value::as_str)
                                .filter(|s| !s.is_empty())
                                .map_or_else(|| partial_json.clone(), str::to_string);
                            if let Some(AssistantContent::ToolCall(tc)) =
                                output.content.get_mut(slot.content_index)
                            {
                                tc.arguments = parse_streaming_json(&args_str);
                                if !sink.toolcall_end(slot.content_index, tc.clone()) {
                                    break 'outer;
                                }
                            }
                        }
                        _ => {}
                    }
                }

                "response.completed" | "response.incomplete" | "response.done" => {
                    saw_terminal_response = true;
                    if let Some(response) = data.get("response") {
                        finalize_response(response, model, &mut output);
                    }
                }

                "response.failed" => {
                    let message = data
                        .get("response")
                        .and_then(|r| r.get("error"))
                        .map_or_else(
                            || "Unknown error (no error details in response)".to_string(),
                            |error| {
                                format!(
                                    "{}: {}",
                                    error
                                        .get("code")
                                        .and_then(Value::as_str)
                                        .unwrap_or("unknown"),
                                    error
                                        .get("message")
                                        .and_then(Value::as_str)
                                        .unwrap_or("no message"),
                                )
                            },
                        );
                    return Err(InferenceError::Other(message));
                }

                "error" => {
                    let code = data.get("code").and_then(Value::as_str).unwrap_or("");
                    let message = data.get("message").and_then(Value::as_str).unwrap_or("");
                    return Err(InferenceError::Other(format!(
                        "Error Code {code}: {message}"
                    )));
                }

                _ => {}
            }
        }

        if done {
            break;
        }
    }

    if !saw_terminal_response {
        return Err(InferenceError::Transport(
            "OpenAI Responses stream ended before a terminal response event".to_string(),
        ));
    }

    finish_output(output, sink);
    Ok(())
}

/// Join the `text` fields of an array of parts with blank lines.
fn join_texts(parts: Option<&Value>, key: &str) -> String {
    parts
        .and_then(Value::as_array)
        .map(|parts| {
            parts
                .iter()
                .filter_map(|p| p.get(key).and_then(Value::as_str))
                .collect::<Vec<_>>()
                .join("\n\n")
        })
        .unwrap_or_default()
}

fn finalize_response(response: &Value, model: &Model, output: &mut AssistantMessage) {
    if let Some(id) = response.get("id").and_then(Value::as_str) {
        output.response_id = Some(id.to_string());
    }
    if let Some(usage) = response.get("usage") {
        let cached = usage
            .get("input_tokens_details")
            .and_then(|d| d.get("cached_tokens"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        let input = usage
            .get("input_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        // OpenAI counts cached tokens inside input_tokens; our unified model
        // keeps them separate, so subtract.
        output.usage.input = input.saturating_sub(cached);
        output.usage.output = usage
            .get("output_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        output.usage.cache_read = cached;
        output.usage.cache_write = 0; // OpenAI has no explicit cache writes.
        output.usage.reasoning = usage
            .get("output_tokens_details")
            .and_then(|d| d.get("reasoning_tokens"))
            .and_then(Value::as_u64);
        output.usage.total_tokens = usage
            .get("total_tokens")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        calculate_cost(model, &mut output.usage);
    }
    output.stop_reason = match response.get("status").and_then(Value::as_str) {
        Some("incomplete")
            if response
                .pointer("/incomplete_details/reason")
                .and_then(Value::as_str)
                == Some("content_filter") =>
        {
            output.error_message = Some(CONTENT_FILTER_MESSAGE.to_string());
            StopReason::Error
        }
        Some("incomplete") => StopReason::Length,
        Some("failed" | "cancelled") => StopReason::Error,
        // completed / in_progress / queued (the last two are wonky but
        // terminal in practice) all map to a clean stop.
        _ => StopReason::Stop,
    };
    // The Responses API has no "tool_use" stop status; infer it.
    if output.stop_reason == StopReason::Stop
        && output
            .content
            .iter()
            .any(|c| matches!(c, AssistantContent::ToolCall(_)))
    {
        output.stop_reason = StopReason::ToolUse;
    }
}

/// Compat knobs for the Responses API, read from `model.compat`.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct OpenAiCompat {
    /// Reasoning models take the system prompt in a `developer` role.
    supports_developer_role: bool,
    /// Whether 24h prompt-cache retention is available.
    supports_long_cache_retention: bool,
    /// Whether the model accepts the `temperature` request field. GPT-6
    /// Astra rejects it.
    supports_temperature: bool,
}

impl Default for OpenAiCompat {
    fn default() -> Self {
        Self {
            supports_developer_role: true,
            supports_long_cache_retention: true,
            supports_temperature: true,
        }
    }
}

fn openai_compat(model: &Model) -> OpenAiCompat {
    compat(model)
}

/// Problems in the knobs this dialect reads; the Codex provider shares them.
pub(crate) fn compat_problems(model: &Model) -> Vec<String> {
    parse_compat::<OpenAiCompat>(model).1
}

/// The one knob the Codex provider shares with this dialect.
pub(crate) fn supports_temperature(model: &Model) -> bool {
    openai_compat(model).supports_temperature
}

fn build_request_body(model: &Model, context: &Context, options: &StreamOptions) -> Value {
    let compat = openai_compat(model);
    let cache_retention = options.cache_retention.unwrap_or(CacheRetention::Short);

    let mut body = json!({
        "model": model.id,
        "input": convert_messages(model, context, &compat),
        "stream": true,
        // Stateless mode: nothing is persisted server-side; reasoning comes
        // back encrypted and is replayed by us (see module docs).
        "store": false,
    });

    // The prompt cache key routes requests with the same prefix to the same
    // cache shard. Session ids serve nicely; the API caps the key at 64 chars.
    if cache_retention != CacheRetention::None
        && let Some(session_id) = &options.session_id
    {
        let key: String = session_id.chars().take(64).collect();
        body["prompt_cache_key"] = json!(key);
        if cache_retention == CacheRetention::Long && compat.supports_long_cache_retention {
            body["prompt_cache_retention"] = json!("24h");
        }
    }

    if let Some(max_tokens) = options.max_tokens {
        body["max_output_tokens"] = json!(clamp_max_tokens_to_context(model, context, max_tokens));
    }
    if let Some(temperature) = options.temperature
        && compat.supports_temperature
    {
        body["temperature"] = json!(temperature);
    }

    if let Some(tools) = &context.tools
        && !tools.is_empty()
    {
        body["tools"] = Value::Array(
            tools
                .iter()
                .map(|tool| {
                    json!({
                        "type": "function",
                        "name": tool.name,
                        "description": tool.description,
                        // Our parameters are already JSON Schema.
                        "parameters": tool.parameters,
                        "strict": false,
                    })
                })
                .collect(),
        );
    }

    if model.reasoning {
        if let Some(effort) = thinking_effort(model, options.reasoning, EffortStyle::OpenAi) {
            body["reasoning"] = json!({"effort": effort, "summary": "auto"});
            // Required in stateless mode so reasoning can be replayed.
            body["include"] = json!(["reasoning.encrypted_content"]);
        } else if let Some(effort) = off_effort(model, Some("none")) {
            body["reasoning"] = json!({"effort": effort});
        }
    }

    body
}

fn convert_messages(model: &Model, context: &Context, compat: &OpenAiCompat) -> Value {
    let mut items: Vec<Value> = Vec::new();

    if let Some(system_prompt) = &context.system_prompt {
        let role = if model.reasoning && compat.supports_developer_role {
            "developer"
        } else {
            "system"
        };
        items.push(json!({"role": role, "content": system_prompt}));
    }

    items.extend(convert_items(model, context, normalize_tool_call_id));
    Value::Array(items)
}

/// The transcript as Responses items without the system prompt. It is shared with the
/// Codex provider, which carries the system prompt in the `instructions` body field
/// instead of a leading message item. The tool-call id normalizer is a parameter
/// because Codex counts the plain `openai` provider as family (ids stay usable
/// across a switch).
pub(crate) fn convert_items(
    model: &Model,
    context: &Context,
    normalize: crate::transform::NormalizeToolCallId,
) -> Vec<Value> {
    let transformed = transform_messages(&context.messages, model, Some(normalize));
    let mut items: Vec<Value> = Vec::new();

    for (msg_index, msg) in transformed.iter().enumerate() {
        match msg {
            Message::User(user) => {
                let content: Vec<Value> = match &user.content {
                    UserContentBody::Text(text) => {
                        vec![json!({"type": "input_text", "text": text})]
                    }
                    UserContentBody::Blocks(blocks) => blocks
                        .iter()
                        .map(|block| match block {
                            UserContent::Text(t) => {
                                json!({"type": "input_text", "text": t.text})
                            }
                            UserContent::Image(image) => json!({
                                "type": "input_image",
                                "detail": "auto",
                                "image_url": format!(
                                    "data:{};base64,{}",
                                    image.mime_type, image.data
                                ),
                            }),
                        })
                        .collect(),
                };
                if !content.is_empty() {
                    items.push(json!({"role": "user", "content": content}));
                }
            }

            Message::Assistant(assistant) => {
                // A previous response from this model id? Ids stay usable.
                // Same provider+api but a different model id needs the item
                // ids dropped so OpenAI's reasoning/function-call pairing
                // validation doesn't fire.
                let is_different_model = assistant.model != model.id
                    && assistant.provider == model.provider
                    && assistant.api == model.api;
                let mut text_block_index = 0_usize;

                for block in &assistant.content {
                    match block {
                        AssistantContent::Thinking(thinking) => {
                            // The signature holds the whole reasoning item.
                            if let Some(signature) = &thinking.thinking_signature
                                && let Ok(item) = serde_json::from_str::<Value>(signature)
                            {
                                items.push(item);
                            }
                        }
                        AssistantContent::Text(text) => {
                            let (id, phase) = text
                                .text_signature
                                .as_deref()
                                .map(parse_text_signature)
                                .unwrap_or_default();
                            // Message items require an id (<= 64 chars).
                            let msg_id = if id.is_empty() {
                                if text_block_index == 0 {
                                    format!("msg_cupel_{msg_index}")
                                } else {
                                    format!("msg_cupel_{msg_index}_{text_block_index}")
                                }
                            } else if id.len() > 64 {
                                format!("msg_{}", short_hash(&id))
                            } else {
                                id
                            };
                            text_block_index += 1;
                            let mut item = json!({
                                "type": "message",
                                "role": "assistant",
                                "content": [{
                                    "type": "output_text",
                                    "text": text.text,
                                    "annotations": [],
                                }],
                                "status": "completed",
                                "id": msg_id,
                            });
                            if let Some(phase) = phase {
                                item["phase"] = json!(phase);
                            }
                            items.push(item);
                        }
                        AssistantContent::ToolCall(tool_call) => {
                            let (call_id, item_id) = tool_call
                                .id
                                .split_once('|')
                                .map_or((tool_call.id.as_str(), None), |(c, i)| (c, Some(i)));
                            let mut item = json!({
                                "type": "function_call",
                                "call_id": call_id,
                                "name": tool_call.name,
                                "arguments": tool_call.arguments.to_string(),
                            });
                            // Omitting the item id for different-model replay
                            // sidesteps the fc/rs pairing validation.
                            if let Some(item_id) = item_id
                                && !(is_different_model && item_id.starts_with("fc_"))
                            {
                                item["id"] = json!(item_id);
                            }
                            items.push(item);
                        }
                    }
                }
            }

            Message::ToolResult(result) => {
                let text: String = result
                    .content
                    .iter()
                    .filter_map(|c| match c {
                        ToolResultContent::Text(t) => Some(t.text.as_str()),
                        ToolResultContent::Image(_) => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let has_images = result
                    .content
                    .iter()
                    .any(|c| matches!(c, ToolResultContent::Image(_)));
                let call_id = result
                    .tool_call_id
                    .split_once('|')
                    .map_or(result.tool_call_id.as_str(), |(c, _)| c);

                let output_value: Value =
                    if has_images && model.input.contains(&crate::types::InputModality::Image) {
                        let mut parts: Vec<Value> = Vec::new();
                        if !text.is_empty() {
                            parts.push(json!({"type": "input_text", "text": text}));
                        }
                        for block in &result.content {
                            if let ToolResultContent::Image(image) = block {
                                parts.push(json!({
                                    "type": "input_image",
                                    "detail": "auto",
                                    "image_url": format!(
                                        "data:{};base64,{}",
                                        image.mime_type, image.data
                                    ),
                                }));
                            }
                        }
                        Value::Array(parts)
                    } else {
                        Value::String(text)
                    };

                items.push(json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": output_value,
                }));
            }
        }
    }

    items
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event_stream::assistant_message_channel;
    use crate::types::{
        ImageContent, InputModality, Message, ModelCost, Provider as ProviderName, ThinkingLevel,
        ThinkingLevelMap, ToolResultMessage, UserMessage, now_ms,
    };

    /// A GPT-6 Astra catalog row as the generator emits it: no off, no
    /// minimal, max kept by omission, and the temperature knob off.
    fn astra_model() -> Model {
        let mut map = ThinkingLevelMap::new();
        map.insert("off".to_string(), None);
        map.insert("minimal".to_string(), None);
        Model {
            id: "gpt-6-astra".to_string(),
            name: "GPT-6 Astra".to_string(),
            api: Api::from(Api::OPENAI_RESPONSES),
            provider: ProviderName::from(ProviderName::OPENAI),
            base_url: "https://api.openai.com/v1".to_string(),
            reasoning: true,
            thinking_level_map: Some(map),
            input: vec![InputModality::Text, InputModality::Image],
            cost: ModelCost::default(),
            context_window: 272_000,
            max_context_window: None,
            max_tokens: 128_000,
            headers: None,
            compat: Some(json!({"supportsTemperature": false})),
        }
    }

    fn context() -> Context {
        Context {
            system_prompt: Some("You are cupel.".to_string()),
            messages: vec![Message::User(UserMessage {
                content: UserContentBody::Text("hi".to_string()),
                timestamp: now_ms(),
            })],
            tools: None,
        }
    }

    fn with_reasoning(level: Option<ThinkingLevel>) -> StreamOptions {
        StreamOptions {
            reasoning: level,
            temperature: Some(0.2),
            ..StreamOptions::default()
        }
    }

    #[test]
    fn tool_result_placeholders_match_the_actual_output() {
        for supports_images in [false, true] {
            let mut model = astra_model();
            if !supports_images {
                model.input = vec![InputModality::Text];
            }
            for (content, expected) in [
                (Vec::new(), json!("(no output)")),
                (
                    vec![ToolResultContent::Text(TextContent::plain(""))],
                    json!("(no output)"),
                ),
                (
                    vec![ToolResultContent::Text(TextContent::plain("file content"))],
                    json!("file content"),
                ),
                (
                    vec![ToolResultContent::Image(ImageContent {
                        data: "abc".into(),
                        mime_type: "image/png".into(),
                    })],
                    if supports_images {
                        json!([
                            {"type": "input_text", "text": "(see attached image)"},
                            {
                                "type": "input_image",
                                "detail": "auto",
                                "image_url": "data:image/png;base64,abc",
                            },
                        ])
                    } else {
                        json!("(tool image omitted: model does not support images)")
                    },
                ),
            ] {
                let context = Context {
                    system_prompt: None,
                    messages: vec![Message::ToolResult(ToolResultMessage {
                        tool_call_id: "call_1|fc_1".into(),
                        tool_name: "read".into(),
                        content,
                        details: None,
                        is_error: false,
                        timestamp: 0,
                    })],
                    tools: None,
                };
                let items = convert_messages(&model, &context, &openai_compat(&model));
                assert_eq!(
                    items,
                    json!([{
                        "type": "function_call_output",
                        "call_id": "call_1",
                        "output": expected,
                    }])
                );
            }
        }
    }

    #[tokio::test]
    async fn incomplete_content_filter_preserves_partial_output_and_usage() {
        let model = astra_model();
        for (incomplete_reason, expected) in [
            ("content_filter", StopReason::Error),
            ("max_output_tokens", StopReason::Length),
        ] {
            let mut output = new_output_message(&model);
            output.content = vec![AssistantContent::Text(TextContent::plain("partial output"))];
            finalize_response(
                &json!({
                    "id": "response-id",
                    "status": "incomplete",
                    "incomplete_details": {"reason": incomplete_reason},
                    "usage": {"input_tokens": 10, "output_tokens": 2, "total_tokens": 12},
                }),
                &model,
                &mut output,
            );
            assert_eq!(output.stop_reason, expected);
            let (stream, sink) = assistant_message_channel();
            finish_output(output, &sink);
            drop(sink);
            let message = match stream.result().await {
                Ok(message) => {
                    assert_eq!(expected, StopReason::Length);
                    message
                }
                Err(crate::error::MessageStreamError::ProviderError { reason, message }) => {
                    assert_eq!(reason, StopReason::Error);
                    assert!(!crate::retry::is_retryable_assistant_error(&message));
                    assert_eq!(
                        message.error_message.as_deref(),
                        Some(CONTENT_FILTER_MESSAGE)
                    );
                    *message
                }
                Err(error) => panic!("unexpected stream error: {error}"),
            };
            assert_eq!(message.response_id.as_deref(), Some("response-id"));
            assert_eq!(message.usage.total_tokens, 12);
            assert_eq!(
                message.content,
                vec![AssistantContent::Text(TextContent::plain("partial output"))]
            );
        }
    }

    #[test]
    fn astra_drops_temperature_but_default_models_keep_it() {
        let body = build_request_body(&astra_model(), &context(), &with_reasoning(None));
        assert!(body.get("temperature").is_none(), "{body}");

        // Without the knob (older rows, user models.json) nothing changes.
        let mut plain = astra_model();
        plain.compat = None;
        let body = build_request_body(&plain, &context(), &with_reasoning(None));
        assert_eq!(body["temperature"], json!(0.2));
    }

    #[test]
    fn astra_reasoning_levels_go_on_the_wire_by_name() {
        let model = astra_model();
        // max is a real effort value now, not clamped to xhigh.
        let body = build_request_body(
            &model,
            &context(),
            &with_reasoning(Some(ThinkingLevel::Max)),
        );
        assert_eq!(
            body["reasoning"],
            json!({"effort": "max", "summary": "auto"})
        );
        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));

        // minimal is unsupported: the clamp walks up to low (OpenAI's own
        // migration advice for none/minimal users).
        let body = build_request_body(
            &model,
            &context(),
            &with_reasoning(Some(ThinkingLevel::Minimal)),
        );
        assert_eq!(body["reasoning"]["effort"], json!("low"));
    }

    #[test]
    fn astra_off_omits_the_reasoning_field() {
        // off -> null in the map: the model cannot be switched off, so the
        // request carries no reasoning field (never effort "none", which
        // Astra rejects) and the server applies its default effort.
        let body = build_request_body(&astra_model(), &context(), &with_reasoning(None));
        assert!(body.get("reasoning").is_none(), "{body}");

        // The GPT-5.6 shape (off -> "none") still sends the explicit none.
        let mut gpt56 = astra_model();
        let mut map = ThinkingLevelMap::new();
        map.insert("off".to_string(), Some("none".to_string()));
        gpt56.thinking_level_map = Some(map);
        let body = build_request_body(&gpt56, &context(), &with_reasoning(None));
        assert_eq!(body["reasoning"], json!({"effort": "none"}));
    }
}
