//! Anthropic Messages API provider.
//!
//! Flow: build JSON request -> POST `/v1/messages` with `stream: true` ->
//! decode the SSE body -> translate Anthropic events into unified
//! [`AssistantMessageEvent`]s. Port of pi's `anthropic-messages.ts`.
//!
//! Where pi exposes two entry points (`stream` with raw Anthropic options and
//! `streamSimple` that maps a unified reasoning level onto them), this
//! provider folds the `streamSimple` mapping in: callers set
//! `StreamOptions::reasoning` and the provider derives the right thinking
//! configuration for the model (adaptive effort vs. token budget).

use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    error::{InferenceError, Result},
    event_stream::{AssistantMessageStream, EventSink, assistant_message_channel},
    json_util::parse_streaming_json,
    model::calculate_cost,
    options_util::{adjust_max_tokens_for_thinking, clamp_max_tokens_to_context},
    provider::Provider,
    providers::{
        apply_custom_headers, error_message, log_completion, new_output_message, with_cancel,
    },
    sse::{ServerSentEvent, SseDecoder},
    transform::transform_messages,
    types::{
        Api, AssistantContent, AssistantMessage, CacheRetention, Context, Message, Model,
        StopReason, StreamOptions, TextContent, ThinkingContent, ThinkingLevel, Tool, ToolCall,
        ToolResultContent, UserContent, UserContentBody,
    },
};

const ANTHROPIC_VERSION: &str = "2023-06-01";
const INTERLEAVED_THINKING_BETA: &str = "interleaved-thinking-2025-05-14";
const FINE_GRAINED_TOOL_STREAMING_BETA: &str = "fine-grained-tool-streaming-2025-05-14";

// ---------------------------------------------------------------------------
// Compat flags
// ---------------------------------------------------------------------------

/// API-specific compatibility knobs, deserialized from `model.compat`.
/// `#[serde(default)]` on the struct means absent fields use `Default`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct AnthropicCompat {
    /// Use `thinking.type: "adaptive"` + `output_config.effort` instead of a
    /// token budget. Newer models (Opus 4.7+, Fable 5) require this.
    force_adaptive_thinking: bool,
    /// Opus 4.7+ rejects a non-default `temperature`.
    supports_temperature: bool,
    /// Whether the endpoint supports `eager_input_streaming` on tools. When
    /// it does not, we request the fine-grained tool streaming beta instead.
    supports_eager_tool_input_streaming: bool,
    /// Whether `cache_control` may be attached to tool definitions.
    supports_cache_control_on_tools: bool,
    /// Whether 1h cache TTL is available.
    supports_long_cache_retention: bool,
    /// Some Anthropic-compatible gateways emit (and accept) empty thinking
    /// signatures; real Anthropic requires converting those blocks to text.
    allow_empty_signature: bool,
    /// Send `x-session-affinity` so a gateway (e.g. Fireworks) routes all of
    /// a session's requests to the same cache shard. Without it requests
    /// still succeed, but prompt-cache hit rates suffer.
    send_session_affinity_headers: bool,
}

impl Default for AnthropicCompat {
    fn default() -> Self {
        Self {
            force_adaptive_thinking: false,
            supports_temperature: true,
            supports_eager_tool_input_streaming: true,
            supports_cache_control_on_tools: true,
            supports_long_cache_retention: true,
            allow_empty_signature: false,
            send_session_affinity_headers: false,
        }
    }
}

fn anthropic_compat(model: &Model) -> AnthropicCompat {
    model
        .compat
        .clone()
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// OAuth "stealth mode" (Claude Code identity)
// ---------------------------------------------------------------------------

/// Claude Code subscription tokens only allow the Claude Code tool names.
/// pi mimics them: outgoing tool names are canonicalized case-insensitively,
/// incoming ones are mapped back to the caller's original names.
const CLAUDE_CODE_TOOLS: &[&str] = &[
    "Read",
    "Write",
    "Edit",
    "Bash",
    "Grep",
    "Glob",
    "AskUserQuestion",
    "EnterPlanMode",
    "ExitPlanMode",
    "KillShell",
    "NotebookEdit",
    "Skill",
    "Task",
    "TaskOutput",
    "TodoWrite",
    "WebFetch",
    "WebSearch",
];

fn is_oauth_token(api_key: &str) -> bool {
    api_key.contains("sk-ant-oat")
}

fn to_claude_code_name(name: &str) -> String {
    CLAUDE_CODE_TOOLS
        .iter()
        .find(|t| t.eq_ignore_ascii_case(name))
        .map_or_else(|| name.to_string(), |t| (*t).to_string())
}

fn from_claude_code_name(name: &str, tools: Option<&[Tool]>) -> String {
    tools
        .and_then(|tools| tools.iter().find(|t| t.name.eq_ignore_ascii_case(name)))
        .map_or_else(|| name.to_string(), |t| t.name.clone())
}

// ---------------------------------------------------------------------------
// Provider
// ---------------------------------------------------------------------------

pub struct AnthropicProvider {
    http: reqwest::Client,
}

impl AnthropicProvider {
    #[must_use]
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
        }
    }
}

impl Default for AnthropicProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl Provider for AnthropicProvider {
    fn api(&self) -> &str {
        Api::ANTHROPIC_MESSAGES
    }

    fn stream(
        &self,
        model: &Model,
        context: Context,
        options: StreamOptions,
    ) -> AssistantMessageStream {
        // Create the channel; hand the stream back, keep the sink for the task.
        let (stream, sink) = assistant_message_channel();

        // The spawned task needs owned data (`'static`); descriptors are cheap
        // to clone, the HTTP client is internally reference-counted.
        let model = model.clone();
        let http = self.http.clone();

        // The whole body is wrapped so *any* error becomes an `Error` event on
        // the stream - the caller-facing contract is "never panic, never
        // reject; report failures in-band".
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

/// The streaming worker. Returns `Err` only for failures *before* a terminal
/// event was emitted; on success it emits `Done` itself and returns `Ok`.
#[tracing::instrument(name = "anthropic_request", skip_all, fields(model = %model.id, provider = %model.provider.as_str()))]
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

    let is_oauth = is_oauth_token(&api_key);
    let compat = anthropic_compat(model);

    // ---- Request body ----------------------------------------------------
    let body = build_request_body(model, context, options, &compat, is_oauth);
    // TRACE only: request bodies contain the user's code and prompts.
    tracing::trace!(body = %body, "request body");
    let url = format!("{}/v1/messages", model.base_url.trim_end_matches('/'));

    // ---- Headers -----------------------------------------------------------
    let mut req = http
        .post(&url)
        .header("content-type", "application/json")
        .header("anthropic-version", ANTHROPIC_VERSION)
        .header("accept", "application/json");

    let has_tools = context.tools.as_ref().is_some_and(|t| !t.is_empty());
    let mut betas: Vec<&str> = Vec::new();
    // Adaptive-thinking models have interleaved thinking built in -> skip beta.
    if !compat.force_adaptive_thinking {
        betas.push(INTERLEAVED_THINKING_BETA);
    }
    // Only needed when the endpoint can't stream tool input eagerly.
    if has_tools && !compat.supports_eager_tool_input_streaming {
        betas.push(FINE_GRAINED_TOOL_STREAMING_BETA);
    }

    if is_oauth {
        // OAuth: Bearer auth + Claude Code identity headers.
        let mut oauth_betas = vec!["claude-code-20250219", "oauth-2025-04-20"];
        oauth_betas.extend(betas.iter().copied());
        req = req
            .header("authorization", format!("Bearer {api_key}"))
            .header("anthropic-beta", oauth_betas.join(","))
            .header("x-app", "cli");
    } else {
        req = req.header("x-api-key", &api_key);
        if !betas.is_empty() {
            req = req.header("anthropic-beta", betas.join(","));
        }
        // Cache-shard affinity for gateways that ask for it (see compat).
        // Only meaningful when caching is on, hence the retention check.
        if compat.send_session_affinity_headers
            && options.cache_retention != Some(CacheRetention::None)
            && let Some(session_id) = &options.session_id
        {
            req = req.header("x-session-affinity", session_id);
        }
    }
    req = apply_custom_headers(req, model, options);
    if let Some(timeout) = options.timeout_ms {
        req = req.timeout(core::time::Duration::from_millis(timeout));
    }

    // ---- Send, racing the cancellation token -------------------------------
    let response = with_cancel(options, req.json(&body).send()).await??;

    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(InferenceError::ApiStatus {
            status: status.as_u16(),
            body,
        });
    }

    // ---- Stream + decode the SSE body --------------------------------------
    let mut output = new_output_message(model);
    if !sink.start() {
        return Ok(()); // Consumer dropped the stream; stop working.
    }

    // Anthropic addresses content blocks by an `index` field. We track the
    // mapping from that index to our position in `output.content`, plus the
    // partial-JSON scratch buffer for tool calls (which must never leak into
    // the persisted message).
    struct BlockState {
        anthropic_index: u64,
        kind: BlockKind,
    }
    enum BlockKind {
        Text,
        Thinking,
        ToolCall { partial_json: String },
    }
    let mut blocks: Vec<BlockState> = Vec::new();
    let find =
        |blocks: &[BlockState], idx: u64| blocks.iter().position(|b| b.anthropic_index == idx);

    use futures_util::StreamExt as _;
    let mut byte_stream = response.bytes_stream();
    let mut decoder = SseDecoder::new();
    let mut events: Vec<ServerSentEvent> = Vec::new();
    let mut saw_message_start = false;
    let mut saw_message_stop = false;

    'outer: loop {
        let chunk = with_cancel(options, byte_stream.next()).await?;
        let done = chunk.is_none();
        match chunk {
            Some(chunk) => decoder.push(&chunk?, &mut events),
            None => decoder.finish(&mut events),
        }

        for sse in events.drain(..) {
            // The server signals hard errors as `event: error`.
            if sse.event.as_deref() == Some("error") {
                return Err(InferenceError::Other(sse.data));
            }
            let Ok(data) = serde_json::from_str::<Value>(&sse.data) else {
                continue;
            };
            let event_type = data.get("type").and_then(Value::as_str).unwrap_or("");

            match event_type {
                "message_start" => {
                    saw_message_start = true;
                    // Capture response id + initial usage. Doing this here
                    // (not only in message_delta) means we keep input counts
                    // even if the stream is aborted early.
                    if let Some(m) = data.get("message") {
                        output.response_id =
                            m.get("id").and_then(Value::as_str).map(str::to_string);
                        let u = m.get("usage");
                        output.usage.input = usage_u64(u, "input_tokens");
                        output.usage.output = usage_u64(u, "output_tokens");
                        output.usage.cache_read = usage_u64(u, "cache_read_input_tokens");
                        output.usage.cache_write = usage_u64(u, "cache_creation_input_tokens");
                        let write_1h = u
                            .and_then(|u| u.get("cache_creation"))
                            .and_then(|c| c.get("ephemeral_1h_input_tokens"))
                            .and_then(Value::as_u64);
                        output.usage.cache_write1h = write_1h;
                        finalize_usage(model, &mut output);
                    }
                }

                "content_block_start" => {
                    let index = data.get("index").and_then(Value::as_u64).unwrap_or(0);
                    let block = data.get("content_block");
                    let block_type = block.and_then(|b| b.get("type")).and_then(Value::as_str);

                    match block_type {
                        Some("text") => {
                            output
                                .content
                                .push(AssistantContent::Text(TextContent::plain("")));
                            blocks.push(BlockState {
                                anthropic_index: index,
                                kind: BlockKind::Text,
                            });
                            if !sink.text_start(output.content.len() - 1) {
                                break 'outer;
                            }
                        }
                        Some("thinking") => {
                            output
                                .content
                                .push(AssistantContent::Thinking(ThinkingContent {
                                    thinking: String::new(),
                                    thinking_signature: Some(String::new()),
                                    redacted: None,
                                }));
                            blocks.push(BlockState {
                                anthropic_index: index,
                                kind: BlockKind::Thinking,
                            });
                            if !sink.thinking_start(output.content.len() - 1) {
                                break 'outer;
                            }
                        }
                        Some("redacted_thinking") => {
                            // The encrypted payload arrives in `data`; keep it
                            // in the signature so multi-turn replay works.
                            let signature = block
                                .and_then(|b| b.get("data"))
                                .and_then(Value::as_str)
                                .map(str::to_string);
                            output
                                .content
                                .push(AssistantContent::Thinking(ThinkingContent {
                                    thinking: "[Reasoning redacted]".to_string(),
                                    thinking_signature: signature,
                                    redacted: Some(true),
                                }));
                            blocks.push(BlockState {
                                anthropic_index: index,
                                kind: BlockKind::Thinking,
                            });
                            if !sink.thinking_start(output.content.len() - 1) {
                                break 'outer;
                            }
                        }
                        Some("tool_use") => {
                            let id = block
                                .and_then(|b| b.get("id"))
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string();
                            let raw_name = block
                                .and_then(|b| b.get("name"))
                                .and_then(Value::as_str)
                                .unwrap_or("");
                            let name = if is_oauth {
                                from_claude_code_name(raw_name, context.tools.as_deref())
                            } else {
                                raw_name.to_string()
                            };
                            output.content.push(AssistantContent::ToolCall(ToolCall {
                                id,
                                name,
                                arguments: json!({}),
                                thought_signature: None,
                            }));
                            blocks.push(BlockState {
                                anthropic_index: index,
                                kind: BlockKind::ToolCall {
                                    partial_json: String::new(),
                                },
                            });
                            if !sink.toolcall_start(output.content.len() - 1) {
                                break 'outer;
                            }
                        }
                        _ => {}
                    }
                }

                "content_block_delta" => {
                    let index = data.get("index").and_then(Value::as_u64).unwrap_or(0);
                    let Some(pos) = find(&blocks, index) else {
                        continue;
                    };
                    let delta = data.get("delta");
                    let delta_type = delta.and_then(|d| d.get("type")).and_then(Value::as_str);

                    match delta_type {
                        Some("text_delta") => {
                            let text = delta
                                .and_then(|d| d.get("text"))
                                .and_then(Value::as_str)
                                .unwrap_or("");
                            if let Some(AssistantContent::Text(block)) = output.content.get_mut(pos)
                            {
                                block.text.push_str(text);
                                if !sink.text_delta(pos, text.to_string()) {
                                    break 'outer;
                                }
                            }
                        }
                        Some("thinking_delta") => {
                            let text = delta
                                .and_then(|d| d.get("thinking"))
                                .and_then(Value::as_str)
                                .unwrap_or("");
                            if let Some(AssistantContent::Thinking(block)) =
                                output.content.get_mut(pos)
                            {
                                block.thinking.push_str(text);
                                if !sink.thinking_delta(pos, text.to_string()) {
                                    break 'outer;
                                }
                            }
                        }
                        Some("input_json_delta") => {
                            let fragment = delta
                                .and_then(|d| d.get("partial_json"))
                                .and_then(Value::as_str)
                                .unwrap_or("");
                            if let Some(BlockState {
                                kind: BlockKind::ToolCall { partial_json },
                                ..
                            }) = blocks.get_mut(pos)
                            {
                                partial_json.push_str(fragment);
                                let parsed = parse_streaming_json(partial_json);
                                if let Some(AssistantContent::ToolCall(tc)) =
                                    output.content.get_mut(pos)
                                {
                                    tc.arguments = parsed;
                                }
                                if !sink.toolcall_delta(pos, fragment.to_string()) {
                                    break 'outer;
                                }
                            }
                        }
                        Some("signature_delta") => {
                            let fragment = delta
                                .and_then(|d| d.get("signature"))
                                .and_then(Value::as_str)
                                .unwrap_or("");
                            if let Some(AssistantContent::Thinking(block)) =
                                output.content.get_mut(pos)
                            {
                                block
                                    .thinking_signature
                                    .get_or_insert_with(String::new)
                                    .push_str(fragment);
                            }
                        }
                        _ => {}
                    }
                }

                "content_block_stop" => {
                    let index = data.get("index").and_then(Value::as_u64).unwrap_or(0);
                    let Some(pos) = find(&blocks, index) else {
                        continue;
                    };
                    match &blocks[pos].kind {
                        BlockKind::Text => {
                            if let Some(AssistantContent::Text(block)) = output.content.get(pos)
                                && !sink.text_end(pos, block.text.clone())
                            {
                                break 'outer;
                            }
                        }
                        BlockKind::Thinking => {
                            if let Some(AssistantContent::Thinking(block)) = output.content.get(pos)
                                && !sink.thinking_end(pos, block.thinking.clone())
                            {
                                break 'outer;
                            }
                        }
                        BlockKind::ToolCall { partial_json } => {
                            // Final parse of the completed JSON; the scratch
                            // buffer lives in `blocks` and is dropped here.
                            let parsed = parse_streaming_json(partial_json);
                            if let Some(AssistantContent::ToolCall(tc)) =
                                output.content.get_mut(pos)
                            {
                                tc.arguments = parsed;
                                if !sink.toolcall_end(pos, tc.clone()) {
                                    break 'outer;
                                }
                            }
                        }
                    }
                }

                "message_delta" => {
                    if let Some(delta) = data.get("delta")
                        && let Some(reason) = delta.get("stop_reason").and_then(Value::as_str)
                    {
                        let (stop_reason, error) =
                            map_stop_reason(reason, delta.get("stop_details"));
                        output.stop_reason = stop_reason;
                        if let Some(error) = error {
                            output.error_message = Some(error);
                        }
                    }
                    // Only update usage fields that are present (not null):
                    // some proxies omit input_tokens here, and we must keep
                    // the value captured at message_start.
                    if let Some(u) = data.get("usage") {
                        if let Some(v) = u.get("input_tokens").and_then(Value::as_u64) {
                            output.usage.input = v;
                        }
                        if let Some(v) = u.get("output_tokens").and_then(Value::as_u64) {
                            output.usage.output = v;
                        }
                        if let Some(v) = u.get("cache_read_input_tokens").and_then(Value::as_u64) {
                            output.usage.cache_read = v;
                        }
                        if let Some(v) =
                            u.get("cache_creation_input_tokens").and_then(Value::as_u64)
                        {
                            output.usage.cache_write = v;
                        }
                        // Reasoning tokens appear in output_tokens_details on
                        // the final delta (subset of output_tokens).
                        if let Some(v) = u
                            .get("output_tokens_details")
                            .and_then(|d| d.get("thinking_tokens"))
                            .and_then(Value::as_u64)
                        {
                            output.usage.reasoning = Some(v);
                        }
                        finalize_usage(model, &mut output);
                    }
                }

                "message_stop" => {
                    saw_message_stop = true;
                }

                _ => {}
            }
        }

        if done {
            break;
        }
    }

    if saw_message_start && !saw_message_stop {
        return Err(InferenceError::Other(
            "Anthropic stream ended before message_stop".to_string(),
        ));
    }
    if matches!(output.stop_reason, StopReason::Error | StopReason::Aborted) {
        return Err(InferenceError::Other(
            output
                .error_message
                .clone()
                .unwrap_or_else(|| "An unknown error occurred".to_string()),
        ));
    }

    let reason = output.stop_reason;
    log_completion(&output);
    let _ = sink.done(reason, output);
    Ok(())
}

/// Anthropic doesn't send `total_tokens`; compute it from the components,
/// then derive cost from the model's pricing.
fn finalize_usage(model: &Model, output: &mut AssistantMessage) {
    output.usage.total_tokens = output.usage.input
        + output.usage.output
        + output.usage.cache_read
        + output.usage.cache_write;
    calculate_cost(model, &mut output.usage);
}

fn usage_u64(usage: Option<&Value>, key: &str) -> u64 {
    usage
        .and_then(|u| u.get(key))
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

fn map_stop_reason(reason: &str, stop_details: Option<&Value>) -> (StopReason, Option<String>) {
    match reason {
        "end_turn" | "pause_turn" | "stop_sequence" => (StopReason::Stop, None),
        "max_tokens" => (StopReason::Length, None),
        "tool_use" => (StopReason::ToolUse, None),
        "refusal" => {
            let explanation = stop_details
                .and_then(|d| d.get("explanation"))
                .and_then(Value::as_str)
                .unwrap_or("The model refused to complete the request")
                .to_string();
            (StopReason::Error, Some(explanation))
        }
        // Content flagged by safety filters.
        "sensitive" => (StopReason::Error, None),
        other => (
            StopReason::Error,
            Some(format!("Unhandled stop reason: {other}")),
        ),
    }
}

// ---------------------------------------------------------------------------
// Request building
// ---------------------------------------------------------------------------

/// The `cache_control` JSON attached to cacheable blocks, or `None` when the
/// caller disabled caching. Prompt caching is what makes multi-turn agent
/// loops affordable: unchanged prefix tokens are re-read at ~10% of the price.
fn cache_control_value(options: &StreamOptions, compat: &AnthropicCompat) -> Option<Value> {
    let retention = options.cache_retention.unwrap_or(CacheRetention::Short);
    match retention {
        CacheRetention::None => None,
        CacheRetention::Long if compat.supports_long_cache_retention => {
            Some(json!({"type": "ephemeral", "ttl": "1h"}))
        }
        _ => Some(json!({"type": "ephemeral"})),
    }
}

fn build_request_body(
    model: &Model,
    context: &Context,
    options: &StreamOptions,
    compat: &AnthropicCompat,
    is_oauth: bool,
) -> Value {
    let cache_control = cache_control_value(options, compat);

    // ---- Thinking configuration + max_tokens -------------------------------
    // This mirrors pi's streamSimple(): the unified reasoning level decides
    // between adaptive effort (new models) and token budgets (older models),
    // and budget-based thinking needs max_tokens head-room.
    let mut thinking: Option<Value> = None;
    let mut output_config: Option<Value> = None;
    let mut thinking_enabled = false;

    let mut max_tokens = clamp_max_tokens_to_context(
        model,
        context,
        options.max_tokens.unwrap_or(model.max_tokens),
    );

    if model.reasoning {
        match options.reasoning {
            Some(level) => {
                thinking_enabled = true;
                if compat.force_adaptive_thinking {
                    // Adaptive: the model decides when/how much to think; we
                    // only steer with an effort level. "summarized" keeps the
                    // display behavior consistent with older Claude 4 models.
                    thinking = Some(json!({"type": "adaptive", "display": "summarized"}));
                    output_config =
                        Some(json!({"effort": map_thinking_level_to_effort(model, level)}));
                } else {
                    let adjusted = adjust_max_tokens_for_thinking(
                        options.max_tokens,
                        model.max_tokens,
                        level,
                        options.thinking_budgets,
                    );
                    max_tokens = clamp_max_tokens_to_context(model, context, adjusted.max_tokens);
                    let budget = adjusted
                        .thinking_budget
                        .min(max_tokens.saturating_sub(1024));
                    thinking = Some(json!({
                        "type": "enabled",
                        "budget_tokens": budget.max(1024),
                        "display": "summarized",
                    }));
                }
            }
            None => {
                // Explicitly disable thinking - unless the model's level map
                // marks "off" as unsupported (entry present but null).
                let off_unsupported = model
                    .thinking_level_map
                    .as_ref()
                    .is_some_and(|m| matches!(m.get("off"), Some(None)));
                if !off_unsupported {
                    thinking = Some(json!({"type": "disabled"}));
                }
            }
        }
    }

    let mut body = json!({
        "model": model.id,
        "messages": convert_messages(context, model, is_oauth, cache_control.as_ref(), compat),
        "max_tokens": max_tokens,
        "stream": true,
    });

    // ---- System prompt ------------------------------------------------------
    // OAuth tokens MUST lead with the Claude Code identity string.
    let mut system: Vec<Value> = Vec::new();
    if is_oauth {
        let mut identity = json!({
            "type": "text",
            "text": "You are Claude Code, Anthropic's official CLI for Claude.",
        });
        if let Some(cc) = &cache_control {
            identity["cache_control"] = cc.clone();
        }
        system.push(identity);
    }
    if let Some(prompt) = &context.system_prompt {
        let mut block = json!({"type": "text", "text": prompt});
        if let Some(cc) = &cache_control {
            block["cache_control"] = cc.clone();
        }
        system.push(block);
    }
    if !system.is_empty() {
        body["system"] = Value::Array(system);
    }

    // Temperature is incompatible with extended thinking and unsupported on
    // Opus 4.7+.
    if let Some(temperature) = options.temperature
        && !thinking_enabled
        && compat.supports_temperature
    {
        body["temperature"] = json!(temperature);
    }

    if let Some(tools) = &context.tools
        && !tools.is_empty()
    {
        let tool_cache_control = compat
            .supports_cache_control_on_tools
            .then_some(cache_control.as_ref())
            .flatten();
        body["tools"] = convert_tools(
            tools,
            is_oauth,
            compat.supports_eager_tool_input_streaming,
            tool_cache_control,
        );
    }

    if let Some(thinking) = thinking {
        body["thinking"] = thinking;
    }
    if let Some(output_config) = output_config {
        body["output_config"] = output_config;
    }

    if let Some(metadata) = &options.metadata
        && let Some(Value::String(user_id)) = metadata.get("user_id")
    {
        body["metadata"] = json!({"user_id": user_id});
    }

    body
}

/// Map the unified thinking level onto an adaptive-thinking effort string.
/// The model's `thinking_level_map` can override the default mapping.
fn map_thinking_level_to_effort(model: &Model, level: ThinkingLevel) -> String {
    let key = match level {
        ThinkingLevel::Minimal => "minimal",
        ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        ThinkingLevel::High => "high",
        ThinkingLevel::XHigh => "xhigh",
    };
    if let Some(Some(mapped)) = model.thinking_level_map.as_ref().and_then(|m| m.get(key)) {
        return mapped.clone();
    }
    match level {
        ThinkingLevel::Minimal | ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        ThinkingLevel::High => "high",
        ThinkingLevel::XHigh => "xhigh",
    }
    .to_string()
}

/// Anthropic requires tool-call ids matching `^[a-zA-Z0-9_-]{1,64}$`.
fn normalize_tool_call_id(id: &str, _model: &Model, _source: &AssistantMessage) -> String {
    let sanitized: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    sanitized.chars().take(64).collect()
}

fn convert_messages(
    context: &Context,
    model: &Model,
    is_oauth: bool,
    cache_control: Option<&Value>,
    compat: &AnthropicCompat,
) -> Value {
    let transformed = transform_messages(&context.messages, model, Some(normalize_tool_call_id));
    let mut params: Vec<Value> = Vec::new();

    let mut i = 0;
    while i < transformed.len() {
        match &transformed[i] {
            Message::User(user) => {
                match &user.content {
                    UserContentBody::Text(text) => {
                        if !text.trim().is_empty() {
                            params.push(json!({"role": "user", "content": text}));
                        }
                    }
                    UserContentBody::Blocks(blocks) => {
                        let converted: Vec<Value> = blocks
                            .iter()
                            .filter_map(|block| match block {
                                UserContent::Text(t) if t.text.trim().is_empty() => None,
                                UserContent::Text(t) => {
                                    Some(json!({"type": "text", "text": t.text}))
                                }
                                UserContent::Image(image) => Some(json!({
                                    "type": "image",
                                    "source": {
                                        "type": "base64",
                                        "media_type": image.mime_type,
                                        "data": image.data,
                                    },
                                })),
                            })
                            .collect();
                        if !converted.is_empty() {
                            params.push(json!({"role": "user", "content": converted}));
                        }
                    }
                }
                i += 1;
            }

            Message::Assistant(assistant) => {
                let mut blocks: Vec<Value> = Vec::new();
                for block in &assistant.content {
                    match block {
                        AssistantContent::Text(text) => {
                            if text.text.trim().is_empty() {
                                continue;
                            }
                            blocks.push(json!({"type": "text", "text": text.text}));
                        }
                        AssistantContent::Thinking(thinking) => {
                            // Redacted thinking: pass the opaque payload back.
                            if thinking.redacted == Some(true) {
                                blocks.push(json!({
                                    "type": "redacted_thinking",
                                    "data": thinking.thinking_signature.clone().unwrap_or_default(),
                                }));
                                continue;
                            }
                            if thinking.thinking.trim().is_empty() {
                                continue;
                            }
                            let signature_missing = thinking
                                .thinking_signature
                                .as_ref()
                                .is_none_or(|s| s.trim().is_empty());
                            if signature_missing {
                                // No signature (e.g. from an aborted stream):
                                // real Anthropic rejects the block, so demote
                                // to plain text unless the endpoint accepts
                                // empty signatures.
                                if compat.allow_empty_signature {
                                    blocks.push(json!({
                                        "type": "thinking",
                                        "thinking": thinking.thinking,
                                        "signature": "",
                                    }));
                                } else {
                                    blocks.push(json!({
                                        "type": "text",
                                        "text": thinking.thinking,
                                    }));
                                }
                            } else {
                                blocks.push(json!({
                                    "type": "thinking",
                                    "thinking": thinking.thinking,
                                    "signature": thinking.thinking_signature,
                                }));
                            }
                        }
                        AssistantContent::ToolCall(tool_call) => {
                            let name = if is_oauth {
                                to_claude_code_name(&tool_call.name)
                            } else {
                                tool_call.name.clone()
                            };
                            blocks.push(json!({
                                "type": "tool_use",
                                "id": tool_call.id,
                                "name": name,
                                "input": tool_call.arguments,
                            }));
                        }
                    }
                }
                if !blocks.is_empty() {
                    params.push(json!({"role": "assistant", "content": blocks}));
                }
                i += 1;
            }

            Message::ToolResult(_) => {
                // Collect ALL consecutive tool results into one user message;
                // some Anthropic-compatible endpoints require this.
                let mut tool_results: Vec<Value> = Vec::new();
                while let Some(Message::ToolResult(result)) = transformed.get(i) {
                    tool_results.push(json!({
                        "type": "tool_result",
                        "tool_use_id": result.tool_call_id,
                        "content": convert_tool_result_content(&result.content),
                        "is_error": result.is_error,
                    }));
                    i += 1;
                }
                params.push(json!({"role": "user", "content": tool_results}));
            }
        }
    }

    // Attach cache_control to the last user message so the whole conversation
    // prefix is cached for the next turn.
    if let Some(cc) = cache_control
        && let Some(last) = params.last_mut()
        && last.get("role").and_then(Value::as_str) == Some("user")
    {
        match last.get_mut("content") {
            Some(Value::Array(blocks)) => {
                if let Some(last_block) = blocks.last_mut() {
                    last_block["cache_control"] = cc.clone();
                }
            }
            Some(content @ Value::String(_)) => {
                // Plain string content can't carry cache_control; convert to
                // a single text block first.
                let text = content.as_str().unwrap_or("").to_string();
                *content = json!([{"type": "text", "text": text, "cache_control": cc}]);
            }
            _ => {}
        }
    }

    Value::Array(params)
}

/// Tool results are either a plain string (text only) or an array of blocks
/// when images are present.
fn convert_tool_result_content(content: &[ToolResultContent]) -> Value {
    let has_images = content
        .iter()
        .any(|c| matches!(c, ToolResultContent::Image(_)));
    if !has_images {
        let text: Vec<&str> = content
            .iter()
            .filter_map(|c| match c {
                ToolResultContent::Text(t) => Some(t.text.as_str()),
                ToolResultContent::Image(_) => None,
            })
            .collect();
        return Value::String(text.join("\n"));
    }

    let mut blocks: Vec<Value> = content
        .iter()
        .map(|c| match c {
            ToolResultContent::Text(t) => json!({"type": "text", "text": t.text}),
            ToolResultContent::Image(image) => json!({
                "type": "image",
                "source": {
                    "type": "base64",
                    "media_type": image.mime_type,
                    "data": image.data,
                },
            }),
        })
        .collect();
    // Image-only results get placeholder text so the model has something to
    // reference.
    if !blocks
        .iter()
        .any(|b| b.get("type").and_then(Value::as_str) == Some("text"))
    {
        blocks.insert(0, json!({"type": "text", "text": "(see attached image)"}));
    }
    Value::Array(blocks)
}

fn convert_tools(
    tools: &[Tool],
    is_oauth: bool,
    supports_eager_input_streaming: bool,
    cache_control: Option<&Value>,
) -> Value {
    let last = tools.len().saturating_sub(1);
    Value::Array(
        tools
            .iter()
            .enumerate()
            .map(|(index, tool)| {
                let name = if is_oauth {
                    to_claude_code_name(&tool.name)
                } else {
                    tool.name.clone()
                };
                let mut converted = json!({
                    "name": name,
                    "description": tool.description,
                    "input_schema": {
                        "type": "object",
                        "properties": tool.parameters.get("properties").cloned()
                            .unwrap_or_else(|| json!({})),
                        "required": tool.parameters.get("required").cloned()
                            .unwrap_or_else(|| json!([])),
                    },
                });
                if supports_eager_input_streaming {
                    converted["eager_input_streaming"] = json!(true);
                }
                // Caching the tool block caches every (unchanged) tool schema.
                if index == last
                    && let Some(cc) = cache_control
                {
                    converted["cache_control"] = cc.clone();
                }
                converted
            })
            .collect(),
    )
}
