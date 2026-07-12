//! `OpenAI` Chat Completions API provider.
//!
//! This is the oldest and most widely
//! cloned LLM wire protocol - Fireworks, Groq, Together, `DeepSeek`, and
//! dozens of other providers expose "OpenAI-compatible" endpoints that speak
//! it. That ubiquity is also its curse: every clone deviates a little, so
//! this file is half protocol and half compatibility knobs.
//!
//! Protocol shape: POST `{base_url}/chat/completions` with `stream: true`;
//! the SSE body carries `ChatCompletionChunk` JSON. Unlike Anthropic's
//! block-indexed events, chunks have ONE choice whose `delta` may carry
//! `content`, a reasoning field, and/or `tool_calls` keyed by their own
//! index - so we accumulate one text block, one thinking block, and a map
//! of tool-call blocks.

use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    error::{InferenceError, Result},
    event_stream::{AssistantMessageStream, EventSink, assistant_message_channel},
    json_util::parse_streaming_json,
    model::{calculate_cost, clamp_thinking_level},
    options_util::clamp_max_tokens_to_context,
    provider::Provider,
    providers::{
        apply_custom_headers, error_message, log_completion, new_output_message, with_cancel,
    },
    sse::{ServerSentEvent, SseDecoder},
    transform::transform_messages,
    types::{
        Api, AssistantContent, AssistantMessage, Context, Message, Model, ModelThinkingLevel,
        StopReason, StreamOptions, TextContent, ThinkingContent, ThinkingLevel, ToolCall,
        ToolResultContent, UserContent, UserContentBody,
    },
};

// ---------------------------------------------------------------------------
// Compat flags
// ---------------------------------------------------------------------------

/// How a model expects its thinking configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
enum ThinkingFormat {
    /// `reasoning_effort: "low" | ...` (the `OpenAI` standard).
    #[default]
    Openai,
    /// `thinking: {type: enabled|disabled}` plus optional `reasoning_effort`.
    Deepseek,
}

/// Compat knobs, deserialized from `model.compat`. Defaults match a
/// well-behaved `OpenAI`-compatible endpoint; entries only exist for
/// deviations.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct CompletionsCompat {
    /// Endpoint accepts `store: false` (rejecting unknown fields is common).
    supports_store: bool,
    /// Reasoning models take the system prompt as a `developer` role.
    supports_developer_role: bool,
    /// Endpoint accepts the `reasoning_effort` parameter.
    supports_reasoning_effort: bool,
    /// Endpoint accepts `stream_options: {include_usage: true}`.
    supports_usage_in_streaming: bool,
    /// Whether tool definitions may carry `strict: false`.
    supports_strict_mode: bool,
    /// `"max_completion_tokens"` (modern) or `"max_tokens"` (legacy clones).
    max_tokens_field: String,
    /// Some providers require `name` on tool-result messages.
    requires_tool_result_name: bool,
    /// Some providers reject a user message directly after tool results.
    requires_assistant_after_tool_result: bool,
    /// Replay thinking as plain text instead of a vendor reasoning field.
    requires_thinking_as_text: bool,
    /// Send session headers so requests hit the same cache shard.
    send_session_affinity_headers: bool,
    thinking_format: ThinkingFormat,
    /// Endpoint requires a Bearer API key. Local servers (ollama,
    /// llama-server) accept anonymous requests - `requiresApiKey: false`
    /// lets a keyless request proceed without an Authorization header.
    requires_api_key: bool,
}

impl Default for CompletionsCompat {
    fn default() -> Self {
        Self {
            supports_store: true,
            supports_developer_role: true,
            supports_reasoning_effort: true,
            supports_usage_in_streaming: true,
            supports_strict_mode: true,
            max_tokens_field: "max_completion_tokens".to_string(),
            requires_tool_result_name: false,
            requires_assistant_after_tool_result: false,
            requires_thinking_as_text: false,
            send_session_affinity_headers: false,
            thinking_format: ThinkingFormat::Openai,
            requires_api_key: true,
        }
    }
}

fn completions_compat(model: &Model) -> CompletionsCompat {
    model
        .compat
        .clone()
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Provider
// ---------------------------------------------------------------------------

pub struct OpenAiCompletionsProvider {
    http: reqwest::Client,
}

impl OpenAiCompletionsProvider {
    #[must_use]
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
        }
    }
}

impl Default for OpenAiCompletionsProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl Provider for OpenAiCompletionsProvider {
    fn api(&self) -> &str {
        Api::OPENAI_COMPLETIONS
    }

    fn stream(
        &self,
        model: &Model,
        context: Context,
        options: StreamOptions,
    ) -> AssistantMessageStream {
        let (stream, sink) = assistant_message_channel();
        let model = model.clone();
        let http = self.http.clone();

        tokio::spawn(async move {
            if let Err(err) = run(&http, &model, &context, &options, &sink).await {
                let reason = if matches!(err, InferenceError::Aborted) {
                    StopReason::Aborted
                } else {
                    StopReason::Error
                };
                tracing::warn!(error = %err, "provider request failed");
                let msg = error_message(&model, reason, err.to_string());
                let _ = sink.error(reason, msg);
            }
        });

        stream
    }
}

// ---------------------------------------------------------------------------
// Worker
// ---------------------------------------------------------------------------

#[tracing::instrument(name = "openai_completions_request", skip_all, fields(model = %model.id, provider = %model.provider.as_str()))]
async fn run(
    http: &reqwest::Client,
    model: &Model,
    context: &Context,
    options: &StreamOptions,
    sink: &EventSink,
) -> Result<()> {
    // Compat is parsed BEFORE key resolution: `requiresApiKey: false`
    // (local servers) turns a missing key from a hard error into a keyless
    // request. A key that IS present is always sent - ollama ignores it,
    // and authenticated proxies keep working.
    let compat = completions_compat(model);
    let api_key = match options.api_key.clone() {
        Some(key) => Some(key),
        None if !compat.requires_api_key => None,
        None => {
            return Err(InferenceError::MissingApiKey(
                model.provider.as_str().to_string(),
            ));
        }
    };

    let body = build_request_body(model, context, options, &compat);
    // TRACE only: request bodies contain the user's code and prompts.
    tracing::trace!(body = %body, "request body");
    let url = format!("{}/chat/completions", model.base_url.trim_end_matches('/'));

    let mut req = http
        .post(&url)
        .header("content-type", "application/json")
        .header("accept", "application/json");
    if let Some(key) = &api_key {
        req = req.header("authorization", format!("Bearer {key}"));
    }
    if compat.send_session_affinity_headers
        && let Some(session_id) = &options.session_id
    {
        req = req
            .header("session_id", session_id)
            .header("x-client-request-id", session_id)
            .header("x-session-affinity", session_id);
    }
    req = apply_custom_headers(req, model, options);
    if let Some(timeout) = options.timeout_ms {
        req = req.timeout(core::time::Duration::from_millis(timeout));
    }

    let response = with_cancel(options, req.json(&body).send()).await??;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(InferenceError::ApiStatus {
            status: status.as_u16(),
            body,
        });
    }

    let mut output = new_output_message(model);
    if !sink.start() {
        return Ok(());
    }

    // ---- Stream state ------------------------------------------------------
    // At most one text block and one thinking block accumulate (chunks have
    // no block indices for those); tool calls are keyed by the API's own
    // per-call `index`.
    let mut text_index: Option<usize> = None;
    let mut thinking_index: Option<usize> = None;
    // tool-call stream index -> (content index, partial JSON scratch).
    // BTreeMap (not HashMap) so the finalization pass below emits
    // `toolcall_end` events in a deterministic, stream order.
    let mut tool_calls: std::collections::BTreeMap<u64, (usize, String)> =
        std::collections::BTreeMap::new();
    let mut saw_finish_reason = false;

    use futures_util::StreamExt as _;
    let mut byte_stream = response.bytes_stream();
    let mut decoder = SseDecoder::new();
    let mut events: Vec<ServerSentEvent> = Vec::new();

    'outer: loop {
        let chunk = with_cancel(options, byte_stream.next()).await?;
        let done = chunk.is_none();
        match chunk {
            Some(chunk) => decoder.push(&chunk?, &mut events),
            None => decoder.finish(&mut events),
        }

        for sse in events.drain(..) {
            // Chat Completions streams end with a literal "[DONE]" sentinel.
            if sse.data.trim() == "[DONE]" {
                continue;
            }
            let Ok(data) = serde_json::from_str::<Value>(&sse.data) else {
                continue;
            };

            // Every chunk repeats the completion id; capture the first.
            if output.response_id.is_none()
                && let Some(id) = data.get("id").and_then(Value::as_str)
            {
                output.response_id = Some(id.to_string());
            }
            if output.response_model.is_none()
                && let Some(served) = data.get("model").and_then(Value::as_str)
                && !served.is_empty()
                && served != model.id
            {
                output.response_model = Some(served.to_string());
            }
            if let Some(usage) = data.get("usage")
                && !usage.is_null()
            {
                parse_usage(usage, model, &mut output);
            }

            let Some(choice) = data
                .get("choices")
                .and_then(Value::as_array)
                .and_then(|c| c.first())
            else {
                continue;
            };

            if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                let (stop_reason, error) = map_stop_reason(reason);
                output.stop_reason = stop_reason;
                if let Some(error) = error {
                    output.error_message = Some(error);
                }
                saw_finish_reason = true;
            }

            let Some(delta) = choice.get("delta") else {
                continue;
            };

            // ---- text ------------------------------------------------------
            if let Some(content) = delta.get("content").and_then(Value::as_str)
                && !content.is_empty()
            {
                let index = match text_index {
                    Some(index) => index,
                    None => {
                        output
                            .content
                            .push(AssistantContent::Text(TextContent::plain("")));
                        let index = output.content.len() - 1;
                        text_index = Some(index);
                        if !sink.text_start(index) {
                            break 'outer;
                        }
                        index
                    }
                };
                if let Some(AssistantContent::Text(block)) = output.content.get_mut(index) {
                    block.text.push_str(content);
                    if !sink.text_delta(index, content.to_string()) {
                        break 'outer;
                    }
                }
            }

            // ---- reasoning --------------------------------------------------
            // Clones disagree on the field name; take the first non-empty one.
            // The field name is stored as the thinking SIGNATURE so replay can
            // write the text back into the same vendor field.
            let reasoning_field = ["reasoning_content", "reasoning", "reasoning_text"]
                .iter()
                .find_map(|field| {
                    delta
                        .get(*field)
                        .and_then(Value::as_str)
                        .filter(|v| !v.is_empty())
                        .map(|v| (*field, v))
                });
            if let Some((field, fragment)) = reasoning_field {
                let index = match thinking_index {
                    Some(index) => index,
                    None => {
                        output
                            .content
                            .push(AssistantContent::Thinking(ThinkingContent {
                                thinking: String::new(),
                                thinking_signature: Some(field.to_string()),
                                redacted: None,
                            }));
                        let index = output.content.len() - 1;
                        thinking_index = Some(index);
                        if !sink.thinking_start(index) {
                            break 'outer;
                        }
                        index
                    }
                };
                if let Some(AssistantContent::Thinking(block)) = output.content.get_mut(index) {
                    block.thinking.push_str(fragment);
                    if !sink.thinking_delta(index, fragment.to_string()) {
                        break 'outer;
                    }
                }
            }

            // ---- tool calls -------------------------------------------------
            if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for call in calls {
                    let stream_index = call.get("index").and_then(Value::as_u64).unwrap_or(0);
                    let entry = match tool_calls.get_mut(&stream_index) {
                        Some(entry) => entry,
                        None => {
                            output.content.push(AssistantContent::ToolCall(ToolCall {
                                id: call
                                    .get("id")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_string(),
                                name: call
                                    .get("function")
                                    .and_then(|f| f.get("name"))
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_string(),
                                arguments: json!({}),
                                thought_signature: None,
                            }));
                            let index = output.content.len() - 1;
                            tool_calls.insert(stream_index, (index, String::new()));
                            if !sink.toolcall_start(index) {
                                break 'outer;
                            }
                            tool_calls
                                .get_mut(&stream_index)
                                .expect("just inserted the entry")
                        }
                    };
                    let (content_index, partial_json) = entry;
                    let content_index = *content_index;

                    // Later chunks can fill in id/name that the first lacked.
                    if let Some(AssistantContent::ToolCall(tc)) =
                        output.content.get_mut(content_index)
                    {
                        if tc.id.is_empty()
                            && let Some(id) = call.get("id").and_then(Value::as_str)
                        {
                            tc.id = id.to_string();
                        }
                        if tc.name.is_empty()
                            && let Some(name) = call
                                .get("function")
                                .and_then(|f| f.get("name"))
                                .and_then(Value::as_str)
                        {
                            tc.name = name.to_string();
                        }
                        let fragment = call
                            .get("function")
                            .and_then(|f| f.get("arguments"))
                            .and_then(Value::as_str)
                            .unwrap_or("");
                        if !fragment.is_empty() {
                            partial_json.push_str(fragment);
                            tc.arguments = parse_streaming_json(partial_json);
                        }
                        if !sink.toolcall_delta(content_index, fragment.to_string()) {
                            break 'outer;
                        }
                    }
                }
            }
        }

        if done {
            break;
        }
    }

    // Chat Completions has no per-block end events; close everything now.
    if let Some(index) = text_index
        && let Some(AssistantContent::Text(block)) = output.content.get(index)
        && !sink.text_end(index, block.text.clone())
    {
        return Ok(());
    }
    if let Some(index) = thinking_index
        && let Some(AssistantContent::Thinking(block)) = output.content.get(index)
        && !sink.thinking_end(index, block.thinking.clone())
    {
        return Ok(());
    }
    for (content_index, partial_json) in tool_calls.values() {
        if let Some(AssistantContent::ToolCall(tc)) = output.content.get_mut(*content_index) {
            tc.arguments = parse_streaming_json(partial_json);
            if !sink.toolcall_end(*content_index, tc.clone()) {
                return Ok(());
            }
        }
    }

    if matches!(output.stop_reason, StopReason::Error | StopReason::Aborted) {
        return Err(InferenceError::Other(
            output
                .error_message
                .clone()
                .unwrap_or_else(|| "Provider returned an error stop reason".to_string()),
        ));
    }
    if !saw_finish_reason {
        return Err(InferenceError::Other(
            "Stream ended without finish_reason".to_string(),
        ));
    }

    let reason = output.stop_reason;
    log_completion(&output);
    let _ = sink.done(reason, output);
    Ok(())
}

fn parse_usage(usage: &Value, model: &Model, output: &mut AssistantMessage) {
    let prompt = usage
        .get("prompt_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    // Standard: prompt_tokens_details.cached_tokens; DeepSeek's older
    // prompt_cache_hit_tokens is the fallback.
    let cache_read = usage
        .get("prompt_tokens_details")
        .and_then(|d| d.get("cached_tokens"))
        .and_then(Value::as_u64)
        .or_else(|| usage.get("prompt_cache_hit_tokens").and_then(Value::as_u64))
        .unwrap_or(0);
    let cache_write = usage
        .get("prompt_tokens_details")
        .and_then(|d| d.get("cache_write_tokens"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    // prompt_tokens INCLUDES cached tokens; our unified model separates them.
    output.usage.input = prompt
        .saturating_sub(cache_read)
        .saturating_sub(cache_write);
    output.usage.output = usage
        .get("completion_tokens")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    output.usage.cache_read = cache_read;
    output.usage.cache_write = cache_write;
    output.usage.reasoning = usage
        .get("completion_tokens_details")
        .and_then(|d| d.get("reasoning_tokens"))
        .and_then(Value::as_u64);
    output.usage.total_tokens = output.usage.input + output.usage.output + cache_read + cache_write;
    calculate_cost(model, &mut output.usage);
}

fn map_stop_reason(reason: &str) -> (StopReason, Option<String>) {
    match reason {
        "stop" | "end" => (StopReason::Stop, None),
        "length" => (StopReason::Length, None),
        "tool_calls" | "function_call" => (StopReason::ToolUse, None),
        other => (
            StopReason::Error,
            Some(format!("Provider finish_reason: {other}")),
        ),
    }
}

// ---------------------------------------------------------------------------
// Request building
// ---------------------------------------------------------------------------

fn build_request_body(
    model: &Model,
    context: &Context,
    options: &StreamOptions,
    compat: &CompletionsCompat,
) -> Value {
    let mut body = json!({
        "model": model.id,
        "messages": convert_messages(model, context, compat),
        "stream": true,
    });

    // Without this, most endpoints omit usage from streaming responses.
    if compat.supports_usage_in_streaming {
        body["stream_options"] = json!({"include_usage": true});
    }
    if compat.supports_store {
        body["store"] = json!(false);
    }

    if let Some(max_tokens) = options.max_tokens {
        let clamped = clamp_max_tokens_to_context(model, context, max_tokens);
        body[compat.max_tokens_field.as_str()] = json!(clamped);
    }
    if let Some(temperature) = options.temperature {
        body["temperature"] = json!(temperature);
    }

    let has_tools = context.tools.as_ref().is_some_and(|t| !t.is_empty());
    if has_tools {
        let tools = context.tools.as_deref().unwrap_or_default();
        body["tools"] = Value::Array(
            tools
                .iter()
                .map(|tool| {
                    let mut function = json!({
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.parameters,
                    });
                    if compat.supports_strict_mode {
                        function["strict"] = json!(false);
                    }
                    json!({"type": "function", "function": function})
                })
                .collect(),
        );
    } else if has_tool_history(&context.messages) {
        // Anthropic-behind-a-proxy requires `tools` whenever the transcript
        // contains tool calls/results, even when none are offered now.
        body["tools"] = json!([]);
    }

    // ---- thinking -----------------------------------------------------------
    if model.reasoning {
        // Clamp to what the model supports, then map through the model's own
        // level -> effort table.
        let requested = options.reasoning.map(|level| match level {
            ThinkingLevel::Minimal => ModelThinkingLevel::Minimal,
            ThinkingLevel::Low => ModelThinkingLevel::Low,
            ThinkingLevel::Medium => ModelThinkingLevel::Medium,
            ThinkingLevel::High => ModelThinkingLevel::High,
            ThinkingLevel::XHigh => ModelThinkingLevel::XHigh,
        });
        let clamped = requested.map(|level| clamp_thinking_level(model, level));
        let effort_on = clamped.filter(|level| *level != ModelThinkingLevel::Off);

        match compat.thinking_format {
            ThinkingFormat::Deepseek => {
                if effort_on.is_some() {
                    body["thinking"] = json!({"type": "enabled"});
                } else {
                    let off_unsupported = model
                        .thinking_level_map
                        .as_ref()
                        .is_some_and(|m| matches!(m.get("off"), Some(None)));
                    if !off_unsupported {
                        body["thinking"] = json!({"type": "disabled"});
                    }
                }
                if let Some(level) = effort_on
                    && compat.supports_reasoning_effort
                {
                    body["reasoning_effort"] = json!(mapped_effort(model, level));
                }
            }
            ThinkingFormat::Openai => {
                if compat.supports_reasoning_effort {
                    if let Some(level) = effort_on {
                        body["reasoning_effort"] = json!(mapped_effort(model, level));
                    } else if let Some(Some(off_value)) = model
                        .thinking_level_map
                        .as_ref()
                        .map(|m| m.get("off").cloned().flatten())
                    {
                        body["reasoning_effort"] = json!(off_value);
                    }
                }
            }
        }
    }

    body
}

/// Apply the model's level -> effort override table, falling back to the
/// level's own name.
fn mapped_effort(model: &Model, level: ModelThinkingLevel) -> String {
    model
        .thinking_level_map
        .as_ref()
        .and_then(|m| m.get(level.as_str()).cloned().flatten())
        .unwrap_or_else(|| level.as_str().to_string())
}

fn has_tool_history(messages: &[Message]) -> bool {
    messages.iter().any(|msg| match msg {
        Message::ToolResult(_) => true,
        Message::Assistant(assistant) => assistant
            .content
            .iter()
            .any(|block| matches!(block, AssistantContent::ToolCall(_))),
        Message::User(_) => false,
    })
}

/// Tool-call ids: pipe-separated ids from the Responses API get reduced to
/// their call half; everything is sanitized and capped at 40 chars.
fn normalize_tool_call_id(id: &str, _model: &Model, _source: &AssistantMessage) -> String {
    let call_id = id.split_once('|').map_or(id, |(call, _)| call);
    call_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(40)
        .collect()
}

fn convert_messages(model: &Model, context: &Context, compat: &CompletionsCompat) -> Value {
    let transformed = transform_messages(&context.messages, model, Some(normalize_tool_call_id));
    let mut params: Vec<Value> = Vec::new();

    if let Some(system_prompt) = &context.system_prompt {
        let role = if model.reasoning && compat.supports_developer_role {
            "developer"
        } else {
            "system"
        };
        params.push(json!({"role": role, "content": system_prompt}));
    }

    let mut last_was_tool_result = false;
    let mut i = 0;
    while i < transformed.len() {
        match &transformed[i] {
            Message::User(user) => {
                if compat.requires_assistant_after_tool_result && last_was_tool_result {
                    params.push(json!({
                        "role": "assistant",
                        "content": "I have processed the tool results.",
                    }));
                }
                last_was_tool_result = false;

                match &user.content {
                    UserContentBody::Text(text) => {
                        params.push(json!({"role": "user", "content": text}));
                    }
                    UserContentBody::Blocks(blocks) => {
                        let content: Vec<Value> = blocks
                            .iter()
                            .map(|block| match block {
                                UserContent::Text(t) => json!({"type": "text", "text": t.text}),
                                UserContent::Image(image) => json!({
                                    "type": "image_url",
                                    "image_url": {"url": format!(
                                        "data:{};base64,{}",
                                        image.mime_type, image.data
                                    )},
                                }),
                            })
                            .collect();
                        if !content.is_empty() {
                            params.push(json!({"role": "user", "content": content}));
                        }
                    }
                }
                i += 1;
            }

            Message::Assistant(assistant) => {
                last_was_tool_result = false;
                let mut message = json!({"role": "assistant"});

                // Assistant text goes as a plain STRING - the standard format.
                // Sending block arrays makes some clones (DeepSeek via NIM)
                // mirror the structure literally in their next answer.
                let text: String = assistant
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        AssistantContent::Text(t) if !t.text.trim().is_empty() => {
                            Some(t.text.as_str())
                        }
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");

                let thinking: Vec<&ThinkingContent> = assistant
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        AssistantContent::Thinking(t) if !t.thinking.trim().is_empty() => Some(t),
                        _ => None,
                    })
                    .collect();

                if thinking.is_empty() {
                    if !text.is_empty() {
                        message["content"] = json!(text);
                    }
                } else if compat.requires_thinking_as_text {
                    // No tags around it - tags teach the model to mimic them.
                    let mut combined: Vec<String> =
                        thinking.iter().map(|t| t.thinking.clone()).collect();
                    if !text.is_empty() {
                        combined.push(text.clone());
                    }
                    message["content"] = json!(combined.join("\n\n"));
                } else {
                    if !text.is_empty() {
                        message["content"] = json!(text);
                    }
                    // The signature IS the vendor field name the reasoning
                    // came from (see the streaming side); write it back there.
                    if let Some(field) = thinking
                        .first()
                        .and_then(|t| t.thinking_signature.as_deref())
                        .filter(|s| !s.is_empty())
                    {
                        let joined: Vec<&str> =
                            thinking.iter().map(|t| t.thinking.as_str()).collect();
                        message[field] = json!(joined.join("\n"));
                    }
                }

                let tool_calls: Vec<Value> = assistant
                    .content
                    .iter()
                    .filter_map(|block| match block {
                        AssistantContent::ToolCall(tc) => Some(json!({
                            "id": tc.id,
                            "type": "function",
                            "function": {
                                "name": tc.name,
                                "arguments": tc.arguments.to_string(),
                            },
                        })),
                        _ => None,
                    })
                    .collect();
                if !tool_calls.is_empty() {
                    message["tool_calls"] = Value::Array(tool_calls);
                }

                // "Either content or tool_calls" - fully empty messages (e.g.
                // from aborted turns) get skipped.
                if message.get("content").is_none() && message.get("tool_calls").is_none() {
                    i += 1;
                    continue;
                }
                params.push(message);
                i += 1;
            }

            Message::ToolResult(_) => {
                // Consecutive results each become a `tool` role message;
                // any images follow as one user message (the tool role only
                // carries text).
                let mut image_parts: Vec<Value> = Vec::new();
                while let Some(Message::ToolResult(result)) = transformed.get(i) {
                    let text: String = result
                        .content
                        .iter()
                        .filter_map(|c| match c {
                            ToolResultContent::Text(t) => Some(t.text.as_str()),
                            ToolResultContent::Image(_) => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    let mut message = json!({
                        "role": "tool",
                        "content": if text.is_empty() { "(see attached image)" } else { &text },
                        "tool_call_id": result.tool_call_id,
                    });
                    if compat.requires_tool_result_name {
                        message["name"] = json!(result.tool_name);
                    }
                    params.push(message);

                    if model.input.contains(&crate::types::InputModality::Image) {
                        for block in &result.content {
                            if let ToolResultContent::Image(image) = block {
                                image_parts.push(json!({
                                    "type": "image_url",
                                    "image_url": {"url": format!(
                                        "data:{};base64,{}",
                                        image.mime_type, image.data
                                    )},
                                }));
                            }
                        }
                    }
                    i += 1;
                }

                if image_parts.is_empty() {
                    last_was_tool_result = true;
                } else {
                    if compat.requires_assistant_after_tool_result {
                        params.push(json!({
                            "role": "assistant",
                            "content": "I have processed the tool results.",
                        }));
                    }
                    let mut content = vec![
                        json!({"type": "text", "text": "Attached image(s) from tool result:"}),
                    ];
                    content.extend(image_parts);
                    params.push(json!({"role": "user", "content": content}));
                    last_was_tool_result = false;
                }
            }
        }
    }

    Value::Array(params)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{InputModality, ModelCost, Provider as ProviderName};

    fn model_with_compat(compat: Option<serde_json::Value>) -> Model {
        Model {
            id: "local".into(),
            name: "Local".into(),
            api: Api::from(Api::OPENAI_COMPLETIONS),
            provider: ProviderName::from("ollama"),
            base_url: "http://localhost:11434/v1".into(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![InputModality::Text],
            cost: ModelCost {
                input: 0.0,
                output: 0.0,
                cached_read: 0.0,
                cached_write: 0.0,
            },
            context_window: 4096,
            max_tokens: 4096,
            headers: None,
            compat,
        }
    }

    #[test]
    fn requires_api_key_defaults_to_true() {
        // No compat at all: a well-behaved cloud endpoint wants a key.
        let compat = completions_compat(&model_with_compat(None));
        assert!(compat.requires_api_key);
    }

    #[test]
    fn requires_api_key_false_parses_from_camel_case() {
        let compat = completions_compat(&model_with_compat(Some(serde_json::json!({
            "requiresApiKey": false,
            "supportsStore": false,
        }))));
        assert!(!compat.requires_api_key);
        assert!(!compat.supports_store);
        // Unmentioned flags keep their defaults.
        assert!(compat.supports_strict_mode);
    }

    #[test]
    fn malformed_compat_falls_back_to_all_defaults() {
        // A type error fails the WHOLE parse, which `.ok()` turns into the
        // defaults - so a typo'd requiresApiKey silently demands a key
        // again. Pinned here so a future change to per-field tolerance is
        // a conscious decision.
        let compat = completions_compat(&model_with_compat(Some(serde_json::json!({
            "requiresApiKey": "nope",
        }))));
        assert!(compat.requires_api_key);
    }
}
