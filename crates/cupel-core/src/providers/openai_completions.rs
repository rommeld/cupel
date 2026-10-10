//! `OpenAI` Chat Completions API provider.
//!
//! This is the oldest and most widely cloned LLM wire protocol. Fireworks,
//! Groq, Together, `DeepSeek`, and dozens of other providers expose
//! "OpenAI-compatible" endpoints that speak it. That ubiquity is also its
//! curse: every clone deviates a little, so
//! this file is half protocol and half compatibility knobs.
//!
//! Protocol shape: POST `{base_url}/chat/completions` with `stream: true`;
//! the SSE body carries `ChatCompletionChunk` JSON. Unlike Anthropic's
//! block-indexed events, chunks have one choice whose `delta` may carry
//! `content`, a reasoning field, and/or `tool_calls` keyed by their own
//! index. We accumulate one text block, one thinking block, and a map
//! of tool-call blocks.

use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    error::{InferenceError, Result},
    event_stream::{AssistantMessageStream, EventSink},
    json_util::parse_streaming_json,
    model::calculate_cost,
    options_util::clamp_max_tokens_to_context,
    provider::Provider,
    providers::{
        CONTENT_FILTER_MESSAGE, EffortStyle, apply_custom_headers, finish_output,
        new_output_message, off_effort, send_request, spawn_provider_stream, thinking_effort,
        with_cancel,
    },
    sse::{ServerSentEvent, SseDecoder},
    transform::transform_messages,
    types::{
        Api, AssistantContent, AssistantMessage, Context, Message, Model, StopReason,
        StreamOptions, TextContent, ThinkingContent, ToolCall, ToolResultContent, UserContent,
        UserContentBody,
    },
};

/// How a model expects its thinking configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
enum ThinkingFormat {
    /// `reasoning_effort: "low" | ...` (the `OpenAI` standard).
    #[default]
    Openai,
    /// `thinking: {type: enabled|disabled}` plus optional `reasoning_effort`.
    Deepseek,
    /// OpenRouter's unified `reasoning: {effort: ...}` object with one scale
    /// the router translates for every vendor behind it.
    Openrouter,
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
    /// llama-server) accept anonymous requests using `requiresApiKey: false`
    /// lets a keyless request proceed without an Authorization header.
    requires_api_key: bool,
    /// Whether the model accepts `temperature` (GPT-6 Astra rejects it;
    /// derived from models.dev by the generator).
    supports_temperature: bool,
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
            supports_temperature: true,
        }
    }
}

fn completions_compat(model: &Model) -> CompletionsCompat {
    model
        .compat
        .clone()
        .map(|value| {
            serde_json::from_value(value).unwrap_or_else(|error| {
                tracing::warn!(
                    model = %model.id,
                    provider = %model.provider.as_str(),
                    error = %error,
                    "invalid OpenAI Completions compat settings; using defaults"
                );
                CompletionsCompat::default()
            })
        })
        .unwrap_or_default()
}

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
        let http = self.http.clone();
        spawn_provider_stream(model, move |model, sink| async move {
            run(&http, &model, &context, &options, &sink).await
        })
    }
}

#[tracing::instrument(name = "openai_completions_request", skip_all, fields(model = %model.id, provider = %model.provider.as_str()))]
async fn run(
    http: &reqwest::Client,
    model: &Model,
    context: &Context,
    options: &StreamOptions,
    sink: &EventSink,
) -> Result<()> {
    // Compat is parsed before key resolution: `requiresApiKey: false`
    // (local servers) turns a missing key from a hard error into a keyless
    // request. A key that is present is always sent. Ollama ignores it,
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

    let response = send_request(req.json(&body), options).await?;

    let mut output = new_output_message(model);
    if !sink.start() {
        return Ok(());
    }

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

            // Gateways can fail inside an otherwise successful HTTP stream,
            // with or without choices. Preserve details before finish_reason
            // handling can replace them with a generic error.
            if let Some(error) = data.get("error").filter(|error| !error.is_null()) {
                let code = match error.get("code") {
                    Some(Value::String(code)) => code.clone(),
                    Some(Value::Number(code)) => code.to_string(),
                    _ => "unknown".to_string(),
                };
                let message = error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("no message");
                return Err(InferenceError::Other(format!(
                    "Error Code {code}: {message}"
                )));
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

            // Clones disagree on the field name; take the first non-empty one.
            // The field name is stored as the thinking signature so replay can
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

    if !saw_finish_reason {
        return Err(InferenceError::Other(
            "Stream ended without finish_reason".to_string(),
        ));
    }

    finish_output(output, sink);
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
    // prompt_tokens includes cached tokens; our unified model separates them.
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
        "content_filter" => (StopReason::Error, Some(CONTENT_FILTER_MESSAGE.to_string())),
        other => (
            StopReason::Error,
            Some(format!("Provider finish_reason: {other}")),
        ),
    }
}

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
    if let Some(temperature) = options.temperature
        && compat.supports_temperature
    {
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

    if model.reasoning {
        let effort = thinking_effort(model, options.reasoning, EffortStyle::OpenAi);

        match compat.thinking_format {
            ThinkingFormat::Deepseek => {
                if effort.is_some() {
                    body["thinking"] = json!({"type": "enabled"});
                } else if off_effort(model, Some("disabled")).is_some() {
                    body["thinking"] = json!({"type": "disabled"});
                }
                if let Some(effort) = &effort
                    && compat.supports_reasoning_effort
                {
                    body["reasoning_effort"] = json!(effort);
                }
            }
            ThinkingFormat::Openai => {
                if compat.supports_reasoning_effort {
                    // Chat Completions omits off unless metadata names an effort.
                    if let Some(effort) = effort.as_deref().or_else(|| off_effort(model, None)) {
                        body["reasoning_effort"] = json!(effort);
                    }
                }
            }
            ThinkingFormat::Openrouter => {
                if let Some(effort) = effort
                    .as_deref()
                    .or_else(|| off_effort(model, Some("none")))
                {
                    body["reasoning"] = json!({"effort": effort});
                }
            }
        }
    }

    body
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

                // Assistant text goes as a plain string in the standard format.
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
                    // No tags around it because tags teach the model to mimic them.
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
                    // The signature is the vendor field name the reasoning
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

                // "Either content or tool_calls" means fully empty messages (e.g.
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
                        "content": text,
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
    use crate::types::{
        ImageContent, InputModality, ModelCost, Provider as ProviderName, ThinkingLevel,
        ToolResultMessage,
    };

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
            cost: ModelCost::default(),
            context_window: 4096,
            max_context_window: None,
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
    fn malformed_compat_warns_before_falling_back_to_defaults() {
        use std::sync::{Arc, Mutex};

        #[derive(Clone)]
        struct LogWriter(Arc<Mutex<Vec<u8>>>);

        impl std::io::Write for LogWriter {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().write(buf)
            }

            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let log = LogWriter(Arc::new(Mutex::new(Vec::new())));
        let writer = log.clone();
        let subscriber = tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_max_level(tracing::Level::WARN)
            .with_writer(move || writer.clone())
            .finish();
        let mut model = model_with_compat(Some(serde_json::json!({"requiresApiKey": "false"})));
        model.id = "bad-llama-compat-fixture".to_string();
        let compat = tracing::subscriber::with_default(subscriber, || completions_compat(&model));
        assert!(compat.requires_api_key);
        let output = String::from_utf8(log.0.lock().unwrap().clone()).unwrap();
        assert!(output.contains(&model.id), "{output}");
        assert!(
            output.contains("invalid OpenAI Completions compat settings"),
            "{output}"
        );
        assert!(output.contains("expected a boolean"), "{output}");
        assert!(output.contains("using defaults"), "{output}");
    }

    /// A reasoning model pinned to the OpenRouter thinking format, with an
    /// optional level -> effort map (mirrors a curated openrouter row).
    fn openrouter_model(map: Option<crate::types::ThinkingLevelMap>) -> Model {
        let mut model = model_with_compat(Some(serde_json::json!({
            "thinkingFormat": "openrouter",
            "supportsDeveloperRole": false,
        })));
        model.reasoning = true;
        model.thinking_level_map = map;
        model
    }

    fn empty_context() -> Context {
        Context {
            system_prompt: None,
            messages: Vec::new(),
            tools: None,
        }
    }

    #[test]
    fn tool_result_placeholders_match_the_actual_output() {
        for supports_images in [false, true] {
            let mut model = model_with_compat(None);
            if supports_images {
                model.input.push(InputModality::Image);
            }
            let compat = completions_compat(&model);
            let mut context = empty_context();
            context.messages = vec![
                Message::ToolResult(ToolResultMessage {
                    tool_call_id: "image_call".into(),
                    tool_name: "read".into(),
                    content: vec![ToolResultContent::Image(ImageContent {
                        data: "abc".into(),
                        mime_type: "image/png".into(),
                    })],
                    details: None,
                    is_error: false,
                    timestamp: 0,
                }),
                Message::ToolResult(ToolResultMessage {
                    tool_call_id: "empty_call".into(),
                    tool_name: "read".into(),
                    content: vec![ToolResultContent::Text(TextContent::plain(""))],
                    details: None,
                    is_error: false,
                    timestamp: 0,
                }),
            ];
            let messages = convert_messages(&model, &context, &compat);
            let image_text = if supports_images {
                "(see attached image)"
            } else {
                "(tool image omitted: model does not support images)"
            };
            assert_eq!(messages[0]["content"], image_text);
            assert_eq!(messages[1]["content"], "(no output)");
            assert_eq!(messages[1]["tool_call_id"], "empty_call");
            assert_eq!(
                messages.as_array().unwrap().len(),
                if supports_images { 3 } else { 2 }
            );
            if supports_images {
                assert_eq!(messages[2]["content"][1]["type"], "image_url");
            }
        }
    }

    #[test]
    fn documented_finish_reasons_keep_their_terminal_states() {
        for (finish, expected) in [
            ("stop", StopReason::Stop),
            ("end", StopReason::Stop),
            ("length", StopReason::Length),
            ("tool_calls", StopReason::ToolUse),
            ("function_call", StopReason::ToolUse),
        ] {
            assert_eq!(map_stop_reason(finish), (expected, None));
        }
    }

    /// Serve a real SSE response with some text followed by the supplied chunk.
    async fn stream_chunk(
        chunk: Value,
    ) -> core::result::Result<AssistantMessage, crate::error::MessageStreamError> {
        stream_chunks(vec![chunk]).await
    }

    async fn stream_chunks(
        chunks: Vec<Value>,
    ) -> core::result::Result<AssistantMessage, crate::error::MessageStreamError> {
        use core::fmt::Write as _;
        use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let tail = chunks.iter().fold(String::new(), |mut tail, chunk| {
            writeln!(tail, "data: {chunk}\n").unwrap();
            tail
        });
        let body = format!(
            "data: {}\n\n{tail}data: [DONE]\n\n",
            json!({"choices": [{"delta": {"content": "partial output"}}]})
        );
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let (reader, mut writer) = socket.into_split();
            let mut reader = tokio::io::BufReader::new(reader);
            let mut content_length = 0;
            loop {
                let mut line = String::new();
                assert!(reader.read_line(&mut line).await.unwrap() > 0);
                if line == "\r\n" {
                    break;
                }
                if let Some((name, value)) = line.split_once(':')
                    && name.eq_ignore_ascii_case("content-length")
                {
                    content_length = value.trim().parse::<usize>().unwrap();
                }
            }
            reader
                .read_exact(&mut vec![0; content_length])
                .await
                .unwrap();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            writer.write_all(response.as_bytes()).await.unwrap();
        });
        let mut model = model_with_compat(Some(json!({"requiresApiKey": false})));
        model.base_url = format!("http://{address}/v1");
        let options = StreamOptions {
            timeout_ms: Some(5_000),
            ..StreamOptions::default()
        };
        let result = OpenAiCompletionsProvider::new()
            .stream(&model, empty_context(), options)
            .result()
            .await;
        server.await.unwrap();
        result
    }

    #[tokio::test]
    async fn content_filter_preserves_partial_output_and_usage() {
        let error = stream_chunks(vec![
            json!({
                "id": "filtered-completion",
                "model": "gemini-served",
                "choices": [{
                    "delta": {"content": " (incomplete)", "reasoning_content": "partial reasoning"},
                    "finish_reason": "content_filter",
                }],
            }),
            // Usage may arrive in a separate chunk after the finish reason.
            json!({
                "choices": [],
                "usage": {
                    "prompt_tokens": 100,
                    "completion_tokens": 12,
                    "prompt_tokens_details": {"cached_tokens": 30},
                },
            }),
        ])
        .await
        .unwrap_err();
        let crate::error::MessageStreamError::ProviderError { reason, message } = error else {
            panic!("expected a provider error");
        };
        assert_eq!(reason, StopReason::Error);
        assert_eq!(message.stop_reason, StopReason::Error);
        assert_eq!(
            message.error_message.as_deref(),
            Some(
                "The provider's content filter stopped the response. Partial output may be incomplete."
            )
        );
        assert!(!crate::retry::is_retryable_assistant_error(&message));
        assert_eq!(message.response_id.as_deref(), Some("filtered-completion"));
        assert_eq!(message.response_model.as_deref(), Some("gemini-served"));
        assert_eq!(message.usage.input, 70);
        assert_eq!(message.usage.output, 12);
        assert_eq!(message.usage.cache_read, 30);
        assert_eq!(message.usage.total_tokens, 112);
        assert_eq!(
            message.content,
            vec![
                AssistantContent::Text(TextContent::plain("partial output (incomplete)")),
                AssistantContent::Thinking(ThinkingContent {
                    thinking: "partial reasoning".into(),
                    thinking_signature: Some("reasoning_content".into()),
                    redacted: None,
                }),
            ]
        );
    }

    #[tokio::test]
    async fn gateway_stream_errors_preserve_code_and_message_for_retry() {
        for (code, message, expected_code, retryable) in [
            (json!(502), "Bad gateway", "502", true),
            (json!("503"), "Service unavailable", "503", true),
            (json!(429), "Too many requests", "429", true),
            (json!(400), "Provider returned error", "400", false),
            (json!(403), "Content filter triggered", "403", false),
            (
                json!("content_filter"),
                "Provider returned error",
                "content_filter",
                false,
            ),
            (
                json!("insufficient_quota"),
                "Quota exceeded",
                "insufficient_quota",
                false,
            ),
            (Value::Null, "Invalid request", "unknown", false),
        ] {
            for with_choices in [false, true] {
                let mut chunk = json!({"error": {"code": code, "message": message}});
                if with_choices {
                    chunk["choices"] = json!([{"delta": {}, "finish_reason": "error"}]);
                }
                let error = stream_chunk(chunk).await.unwrap_err();
                let crate::error::MessageStreamError::ProviderError {
                    reason,
                    message: output,
                } = error
                else {
                    panic!("expected a provider error");
                };
                assert_eq!(reason, StopReason::Error);
                assert_eq!(output.stop_reason, StopReason::Error);
                assert_eq!(
                    output.error_message.as_deref(),
                    Some(format!("Error Code {expected_code}: {message}").as_str())
                );
                assert_eq!(
                    crate::retry::is_retryable_assistant_error(&output),
                    retryable
                );
            }
        }
    }

    #[tokio::test]
    async fn null_stream_error_does_not_override_a_successful_choice() {
        let output = stream_chunk(json!({
            "error": null,
            "choices": [{"delta": {}, "finish_reason": "stop"}],
        }))
        .await
        .unwrap();
        assert_eq!(output.stop_reason, StopReason::Stop);
        assert!(output.error_message.is_none());
        assert_eq!(
            output.content,
            vec![AssistantContent::Text(TextContent::plain("partial output"))]
        );
    }

    #[test]
    fn openrouter_thinking_rides_the_nested_reasoning_object() {
        let model = openrouter_model(None);
        let compat = completions_compat(&model);
        let options = StreamOptions {
            reasoning: Some(ThinkingLevel::Medium),
            ..StreamOptions::default()
        };
        let body = build_request_body(&model, &empty_context(), &options, &compat);
        // Nested object, not the flat OpenAI reasoning_effort field.
        assert_eq!(body["reasoning"], json!({"effort": "medium"}));
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn openrouter_off_sends_an_explicit_none() {
        // Without an `off -> null` map entry, off is an explicit
        // {effort: "none"} so OpenRouter disables reasoning server-side.
        let model = openrouter_model(None);
        let compat = completions_compat(&model);
        let options = StreamOptions::default();
        let body = build_request_body(&model, &empty_context(), &options, &compat);
        assert_eq!(body["reasoning"], json!({"effort": "none"}));
    }

    /// GPT-6 Astra through an OpenAI-compatible completions endpoint
    /// (OpenRouter's `openai/gpt-6-astra`, or api.openai.com itself).
    fn astra_model(format: &str) -> Model {
        let mut map = crate::types::ThinkingLevelMap::new();
        map.insert("off".to_string(), None);
        map.insert("minimal".to_string(), None);
        let mut model = model_with_compat(Some(serde_json::json!({
            "thinkingFormat": format,
            "supportsTemperature": false,
        })));
        model.reasoning = true;
        model.thinking_level_map = Some(map);
        model
    }

    #[test]
    fn astra_drops_temperature_and_sends_max_effort() {
        for (format, field) in [("openai", "reasoning_effort"), ("openrouter", "reasoning")] {
            let model = astra_model(format);
            let compat = completions_compat(&model);
            let options = StreamOptions {
                reasoning: Some(ThinkingLevel::Max),
                temperature: Some(0.2),
                ..StreamOptions::default()
            };
            let body = build_request_body(&model, &empty_context(), &options, &compat);
            assert!(body.get("temperature").is_none(), "{format}: {body}");
            let effort = if format == "openai" {
                body[field].clone()
            } else {
                body[field]["effort"].clone()
            };
            assert_eq!(effort, json!("max"), "{format}");

            // off -> null: no reasoning field at all, in both formats.
            let body =
                build_request_body(&model, &empty_context(), &StreamOptions::default(), &compat);
            assert!(body.get(field).is_none(), "{format}: {body}");
        }
    }

    #[test]
    fn openrouter_off_is_omitted_for_always_thinking_models() {
        // Kimi K2.7 Code cannot stop thinking (curation pins off -> null);
        // the request must omit the parameter entirely, not send "none".
        let mut map = crate::types::ThinkingLevelMap::new();
        map.insert("off".to_string(), None);
        let model = openrouter_model(Some(map));
        let compat = completions_compat(&model);
        let options = StreamOptions::default();
        let body = build_request_body(&model, &empty_context(), &options, &compat);
        assert!(body.get("reasoning").is_none());
    }
}
