//! Google Gemini GenerateContent adapter. SSE chunks contain complete function
//! calls (argument streaming is deliberately not enabled). Thought signatures
//! stay attached to their original content blocks for stateless replay.

use std::collections::HashSet;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::error::{InferenceError, Result};
use crate::event_stream::{AssistantMessageStream, EventSink};
use crate::model;
use crate::options_util;
use crate::provider::Provider;
use crate::providers;
use crate::sse::{ServerSentEvent, SseDecoder};
use crate::transform;
use crate::types::{
    Api, AssistantContent, AssistantMessage, Context, Message, Model, ModelThinkingLevel,
    StopReason, StreamOptions, TextContent, ThinkingContent, ToolCall, ToolResultContent, Usage,
    UserContent, UserContentBody,
};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct GoogleCompat {
    supports_temperature: bool,
}

impl Default for GoogleCompat {
    fn default() -> Self {
        Self {
            supports_temperature: true,
        }
    }
}

/// Native Gemini Developer API provider, authenticated with an API key.
pub struct GoogleProvider {
    http: reqwest::Client,
}

impl GoogleProvider {
    #[must_use]
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::new(),
        }
    }
}

impl Default for GoogleProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl Provider for GoogleProvider {
    fn api(&self) -> &str {
        Api::GOOGLE_GENERATIVE_AI
    }

    fn stream(
        &self,
        model: &Model,
        context: Context,
        options: StreamOptions,
    ) -> AssistantMessageStream {
        let http = self.http.clone();
        providers::spawn_provider_stream(model, move |model, sink| async move {
            run(&http, &model, &context, &options, &sink).await
        })
    }

    fn compat_problems(&self, model: &Model) -> Vec<String> {
        providers::parse_compat::<GoogleCompat>(model).1
    }
}

#[tracing::instrument(name = "google_request", skip_all, fields(model = %model.id, provider = %model.provider.as_str()))]
async fn run(
    http: &reqwest::Client,
    model: &Model,
    context: &Context,
    options: &StreamOptions,
    sink: &EventSink,
) -> Result<()> {
    let key = options
        .api_key
        .as_deref()
        .ok_or_else(|| InferenceError::MissingApiKey(model.provider.as_str().to_string()))?;
    let compat = providers::compat(model);
    let mut body = build_request_body(model, context, options, &compat);
    tracing::trace!(body = %body, "request body");
    let output_limit = body["generationConfig"]["maxOutputTokens"]
        .as_u64()
        .unwrap_or(model.max_tokens);
    let long_decode = matches!(
        model.id.strip_prefix("models/").unwrap_or(&model.id),
        "gemini-4-argon" | "gemini-4-carbon"
    ) && output_limit > 65_536;
    let url = format!(
        "{}/models/{}:streamGenerateContent?alt=sse",
        model.base_url.trim_end_matches('/'),
        model.id.strip_prefix("models/").unwrap_or(&model.id),
    );
    let mut state = GoogleStream {
        output: providers::new_output_message(model),
        active: None,
        initial_prompt: None,
    };
    let mut seen_tokens = HashSet::new();
    let mut reconnects = 0;
    let mut started = false;
    let failure = loop {
        // Keep the key out of the URL, including transport-error messages.
        let request = http
            .post(&url)
            .header("x-goog-api-key", key)
            .header("accept", "text/event-stream");
        let mut request = providers::apply_custom_headers(request, model, options);
        // Google allows 20 minutes per streaming hop. An explicit caller
        // timeout still wins, and is renewed for each continuation request.
        if let Some(timeout) = options.timeout_ms.or(long_decode.then_some(1_200_000)) {
            request = request.timeout(core::time::Duration::from_millis(timeout));
        }
        let response = match providers::send_request(request.json(&body), options).await {
            Ok(response) => response,
            Err(error) => break Some(error),
        };
        if !started {
            if !sink.start() {
                return Ok(());
            }
            started = true;
        }
        let mut hop = GoogleHop::new(long_decode);
        let result = read_response(
            response,
            options,
            sink,
            &mut state,
            &mut hop,
            &mut seen_tokens,
        )
        .await;
        if matches!(result, Ok(false)) {
            return Ok(()); // Consumer dropped the stream.
        }
        // Usage is cumulative inside a hop. A dropped long-decode stream is
        // billed only through its last checkpoint, even if more was displayed.
        let usage = if result.is_err() && long_decode {
            hop.checkpoint_usage.as_ref()
        } else {
            hop.usage.as_ref()
        };
        if let Some(usage) = usage {
            state.record_usage(model, usage);
        }
        if !state.close_block(sink) {
            return Ok(());
        }
        if let Err(error) = result {
            if !long_decode
                || hop.output_since_checkpoint
                || hop
                    .finish_reason
                    .as_deref()
                    .is_some_and(|reason| reason != "CONTINUATION")
                || !is_stream_drop(&error)
            {
                break Some(error);
            }
            if options.max_retries == Some(0) {
                break Some(error);
            }
            if let Some(token) = hop.continuation_token {
                // Everything emitted is covered by this checkpoint. Starting
                // here cannot replay visible text, thinking or tool calls.
                body["continuationToken"] = json!(token);
                reconnects = 0;
            } else {
                if reconnects >= options.max_retries.unwrap_or(3) {
                    break Some(error);
                }
                reconnects += 1;
            }
            continue;
        }
        if hop.finish_reason.as_deref() == Some("CONTINUATION") && long_decode {
            if hop.output_since_checkpoint {
                break Some(InferenceError::Other(
                    "Google continuation does not cover all streamed output".to_string(),
                ));
            }
            if state.output.usage.output >= output_limit {
                state.output.stop_reason = StopReason::Length;
                break None;
            }
            let Some(token) = hop.continuation_token.filter(|_| hop.terminal_checkpoint) else {
                break Some(InferenceError::Other(
                    "Google continuation ended without a new continuation token".to_string(),
                ));
            };
            if hop.usage.as_ref().is_none_or(|usage| usage.output == 0) {
                break Some(InferenceError::Other(
                    "Google continuation did not report generated token usage".to_string(),
                ));
            }
            // Only this top-level field changes. In particular, keep the same
            // total output budget and never append partial output to contents.
            body["continuationToken"] = json!(token);
            reconnects = 0;
            continue;
        }
        state.finish(hop.finish_reason.as_deref().unwrap_or("UNKNOWN"));
        break None;
    };
    if let Some(error) = failure {
        // All consumers receive append-only events. The outer agent must not
        // restart the whole request after even checkpointed output escaped.
        let error = if long_decode
            && !state.output.content.is_empty()
            && !matches!(error, InferenceError::Aborted)
        {
            InferenceError::NonReplayable(format!(
                "Google generation interrupted after streaming output; automatic replay is unsafe: {error}"
            ))
        } else {
            error
        };
        state.output.stop_reason = if matches!(error, InferenceError::Aborted) {
            StopReason::Aborted
        } else {
            StopReason::Error
        };
        state.output.error_kind = Some(error.kind());
        state.output.error_message = Some(error.to_string());
    }
    let _ = state.close_block(sink);
    providers::finish_output(state.output, sink);
    Ok(())
}

fn build_request_body(
    model: &Model,
    context: &Context,
    options: &StreamOptions,
    compat: &GoogleCompat,
) -> Value {
    let max_tokens = options_util::clamp_max_tokens_to_context(
        model,
        context,
        options
            .max_tokens
            .unwrap_or(model.max_tokens)
            .min(model.max_tokens),
    );
    let mut config = json!({"maxOutputTokens": max_tokens});
    if compat.supports_temperature
        && let Some(temperature) = options.temperature
    {
        config["temperature"] = json!(temperature);
    }
    if model.reasoning {
        let requested = model::effective_thinking_level(model, options.reasoning)
            .map_or(ModelThinkingLevel::Minimal, ModelThinkingLevel::from);
        let level = model::clamp_thinking_level_with(model, requested, |level| {
            matches!(
                level,
                ModelThinkingLevel::Minimal
                    | ModelThinkingLevel::Low
                    | ModelThinkingLevel::Medium
                    | ModelThinkingLevel::High
            )
        });
        let effort = model
            .thinking_level_map
            .as_ref()
            .and_then(|map| map.get(level.as_str()))
            .and_then(Option::as_deref)
            .unwrap_or(level.as_str());
        config["thinkingConfig"] = json!({
            "thinkingLevel": effort.to_ascii_uppercase(), "includeThoughts": true,
        });
    }
    let messages = transform::transform_messages(&context.messages, model, None);
    let mut contents: Vec<Value> = Vec::new();
    for message in &messages {
        let (role, parts) = message_parts(message);
        if parts.is_empty() {
            continue;
        }
        // Parallel function results belong in one user turn, after all calls.
        if let Some(last) = contents.last_mut()
            && last["role"] == role
            && let Some(previous) = last["parts"].as_array_mut()
        {
            previous.extend(parts);
        } else {
            contents.push(json!({"role": role, "parts": parts}));
        }
    }
    let mut body = json!({"contents": contents, "generationConfig": config});
    if let Some(system) = &context.system_prompt {
        body["systemInstruction"] = json!({"parts": [{"text": system}]});
    }
    if let Some(tools) = &context.tools
        && !tools.is_empty()
    {
        let declarations: Vec<Value> = tools
            .iter()
            .map(|tool| {
                json!({
                    "name": tool.name,
                    "description": tool.description,
                    "parametersJsonSchema": tool.parameters,
                })
            })
            .collect();
        body["tools"] = json!([{"functionDeclarations": declarations}]);
    }
    body
}

fn message_parts(message: &Message) -> (&'static str, Vec<Value>) {
    match message {
        Message::User(user) => (
            "user",
            match &user.content {
                UserContentBody::Text(text) => vec![json!({"text": text})],
                UserContentBody::Blocks(blocks) => blocks
                    .iter()
                    .map(|block| match block {
                        UserContent::Text(text) => json!({"text": text.text}),
                        UserContent::Image(image) => json!({"inlineData": {
                            "mimeType": image.mime_type, "data": image.data,
                        }}),
                    })
                    .collect(),
            },
        ),
        Message::Assistant(assistant) => {
            let mut first_call = true;
            let parts = assistant
                .content
                .iter()
                .map(|block| {
                    let (mut part, signature) = match block {
                        AssistantContent::Text(text) => {
                            (json!({"text": text.text}), text.text_signature.as_deref())
                        }
                        AssistantContent::Thinking(thinking) => (
                            json!({"text": thinking.thinking, "thought": true}),
                            thinking.thinking_signature.as_deref(),
                        ),
                        AssistantContent::ToolCall(call) => {
                            // Google's documented escape hatch for imported history
                            // without signatures, e.g. switching from another model.
                            let signature = call.thought_signature.as_deref().or_else(|| {
                                first_call.then_some("skip_thought_signature_validator")
                            });
                            first_call = false;
                            (
                                json!({"functionCall": {
                                    "id": call.id, "name": call.name, "args": call.arguments,
                                }}),
                                signature,
                            )
                        }
                    };
                    if let Some(signature) = signature {
                        part["thoughtSignature"] = json!(signature);
                    }
                    part
                })
                .collect();
            ("model", parts)
        }
        Message::ToolResult(result) => {
            let mut texts = Vec::new();
            let mut images = Vec::new();
            for block in &result.content {
                match block {
                    ToolResultContent::Text(text) => texts.push(text.text.as_str()),
                    ToolResultContent::Image(image) => images.push(json!({"inlineData": {
                        "mimeType": image.mime_type, "data": image.data,
                    }})),
                }
            }
            let key = if result.is_error { "error" } else { "output" };
            let mut response = json!({
                "id": result.tool_call_id, "name": result.tool_name,
                "response": {key: texts.join("\n")},
            });
            if !images.is_empty() {
                response["parts"] = json!(images);
            }
            ("user", vec![json!({"functionResponse": response})])
        }
    }
}

struct GoogleStream {
    output: AssistantMessage,
    active: Option<usize>,
    initial_prompt: Option<u64>,
}

/// Stream immediately, but only resume when the checkpoint covers every emitted
/// part. Consumers cannot retract output that a recovery request may regenerate.
struct GoogleHop {
    long_decode: bool,
    output_since_checkpoint: bool,
    finish_reason: Option<String>,
    continuation_token: Option<String>,
    terminal_checkpoint: bool,
    usage: Option<Usage>,
    checkpoint_usage: Option<Usage>,
}

impl GoogleHop {
    fn new(long_decode: bool) -> Self {
        Self {
            long_decode,
            output_since_checkpoint: false,
            finish_reason: None,
            continuation_token: None,
            terminal_checkpoint: false,
            usage: None,
            checkpoint_usage: None,
        }
    }

    fn consume(
        &mut self,
        data: &Value,
        state: &mut GoogleStream,
        sink: &EventSink,
        seen_tokens: &mut HashSet<String>,
    ) -> Result<bool> {
        // API errors are not dropped-connection recovery signals.
        if data.get("error").is_some() {
            return state.consume(data, sink);
        }
        if let Some(usage) = data.get("usageMetadata") {
            self.usage = Some(parse_usage(usage));
        }
        if let Some(reason) = data
            .pointer("/promptFeedback/blockReason")
            .and_then(Value::as_str)
            && reason != "BLOCK_REASON_UNSPECIFIED"
        {
            self.finish_reason = Some(reason.to_string());
        }
        let candidate = data
            .get("candidates")
            .and_then(Value::as_array)
            .and_then(|candidates| candidates.first());
        if let Some(reason) = candidate
            .and_then(|candidate| candidate.get("finishReason"))
            .and_then(Value::as_str)
        {
            self.finish_reason = Some(reason.to_string());
            self.terminal_checkpoint = false;
        }
        let token = candidate
            .and_then(|candidate| candidate.get("continuationToken"))
            .and_then(Value::as_str)
            .filter(|token| !token.trim().is_empty());
        let token = token.filter(|_| self.long_decode);
        let new_checkpoint =
            token.is_some_and(|token| self.continuation_token.as_deref() != Some(token));
        if new_checkpoint
            && let Some(token) = token
            && !seen_tokens.insert(token.to_string())
        {
            return Err(InferenceError::Other(
                "Google returned an already used continuation token".to_string(),
            ));
        }
        if candidate
            .and_then(|candidate| candidate.pointer("/content/parts"))
            .and_then(Value::as_array)
            .is_some_and(|parts| !parts.is_empty())
        {
            // Includes empty signed text and thinking/tool parts: they affect
            // replay even when no visible text delta was emitted.
            self.output_since_checkpoint = true;
        }
        if !state.consume(data, sink)? {
            return Ok(false);
        }
        if let Some(token) = token {
            if new_checkpoint {
                self.continuation_token = Some(token.to_string());
                self.checkpoint_usage = self.usage.clone();
                self.output_since_checkpoint = false;
            }
            // Repeating a token cannot acknowledge output produced since it.
            self.terminal_checkpoint = self.finish_reason.as_deref() == Some("CONTINUATION")
                && !self.output_since_checkpoint;
        }
        // Final usage can arrive after the terminal token. That token covers
        // the completed hop, unlike intermediate checkpoints during thinking.
        if self.terminal_checkpoint
            && !self.output_since_checkpoint
            && data.get("usageMetadata").is_some()
        {
            self.checkpoint_usage = self.usage.clone();
        }
        Ok(true)
    }
}

async fn read_response(
    response: reqwest::Response,
    options: &StreamOptions,
    sink: &EventSink,
    state: &mut GoogleStream,
    hop: &mut GoogleHop,
    seen_tokens: &mut HashSet<String>,
) -> Result<bool> {
    use futures_util::StreamExt as _;
    let mut stream = response.bytes_stream();
    let mut decoder = SseDecoder::new();
    let mut events: Vec<ServerSentEvent> = Vec::new();
    loop {
        let chunk = providers::with_cancel(options, stream.next()).await?;
        let done = chunk.is_none();
        match chunk {
            Some(chunk) => decoder.push(&chunk?, &mut events),
            None => decoder.finish(&mut events),
        }
        for event in events.drain(..) {
            let data: Value = serde_json::from_str(&event.data).map_err(|error| {
                InferenceError::Transport(format!("Invalid Google stream JSON: {error}"))
            })?;
            if !hop.consume(&data, state, sink, seen_tokens)? {
                return Ok(false);
            }
        }
        // A final signature or usage-only chunk can follow finishReason.
        if done {
            break;
        }
    }
    hop.finish_reason.as_deref().ok_or_else(|| {
        InferenceError::Transport("Google stream ended without a finish reason".to_string())
    })?;
    Ok(true)
}

fn is_stream_drop(error: &InferenceError) -> bool {
    matches!(
        error,
        InferenceError::Transport(_) | InferenceError::Http(_)
    )
}

fn parse_usage(usage: &Value) -> Usage {
    let count = |name| usage.get(name).and_then(Value::as_u64).unwrap_or(0);
    let cached = count("cachedContentTokenCount");
    let output = count("candidatesTokenCount").saturating_add(count("thoughtsTokenCount"));
    Usage {
        input: count("promptTokenCount").saturating_sub(cached),
        cache_read: cached,
        output,
        reasoning: usage.get("thoughtsTokenCount").and_then(Value::as_u64),
        total_tokens: usage
            .get("totalTokenCount")
            .and_then(Value::as_u64)
            .unwrap_or_else(|| count("promptTokenCount").saturating_add(output)),
        ..Usage::default()
    }
}

impl GoogleStream {
    fn finish(&mut self, reason: &str) {
        self.output.stop_reason = match reason {
            "STOP"
                if self
                    .output
                    .content
                    .iter()
                    .any(|block| matches!(block, AssistantContent::ToolCall(_))) =>
            {
                StopReason::ToolUse
            }
            "STOP" => StopReason::Stop,
            "MAX_TOKENS" | "CONTINUATION" => StopReason::Length,
            _ => {
                self.output.error_message = Some(format!("Google stopped generation: {reason}"));
                StopReason::Error
            }
        };
    }

    fn record_usage(&mut self, model: &Model, usage: &Usage) {
        let initial_prompt = *self
            .initial_prompt
            .get_or_insert(usage.input.saturating_add(usage.cache_read));
        let mut hop = usage.clone();
        // Price each request separately; adding input first would incorrectly
        // apply a long-context pricing tier to the combined billed input.
        model::calculate_cost(model, &mut hop);
        let total = &mut self.output.usage;
        total.input = total.input.saturating_add(hop.input);
        total.cache_read = total.cache_read.saturating_add(hop.cache_read);
        total.output = total.output.saturating_add(hop.output);
        total.total_tokens = total.total_tokens.saturating_add(hop.total_tokens);
        if let Some(reasoning) = hop.reasoning {
            total.reasoning = Some(total.reasoning.unwrap_or(0).saturating_add(reasoning));
        }
        total.context_tokens = Some(initial_prompt.saturating_add(total.output));
        total.cost.input += hop.cost.input;
        total.cost.output += hop.cost.output;
        total.cost.cache_read += hop.cost.cache_read;
        total.cost.total += hop.cost.total;
    }

    fn consume(&mut self, data: &Value, sink: &EventSink) -> Result<bool> {
        if let Some(error) = data.get("error") {
            let body = error
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("Google stream returned an error")
                .to_string();
            return Err(
                match error
                    .get("code")
                    .and_then(Value::as_u64)
                    .and_then(|code| u16::try_from(code).ok())
                {
                    Some(status) => InferenceError::ApiStatus { status, body },
                    None => InferenceError::Other(body),
                },
            );
        }
        if let Some(id) = data.get("responseId").and_then(Value::as_str) {
            self.output.response_id = Some(id.to_string());
        }
        if let Some(model) = data.get("modelVersion").and_then(Value::as_str) {
            self.output.response_model = Some(model.to_string());
        }
        if let Some(candidate) = data
            .get("candidates")
            .and_then(Value::as_array)
            .and_then(|candidates| candidates.first())
            && let Some(parts) = candidate
                .pointer("/content/parts")
                .and_then(Value::as_array)
        {
            for (position, part) in parts.iter().enumerate() {
                if !self.part(part, position == 0, sink)? {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }

    fn part(&mut self, part: &Value, may_continue: bool, sink: &EventSink) -> Result<bool> {
        let signature = part.get("thoughtSignature").and_then(Value::as_str);
        if let Some(call) = part.get("functionCall") {
            if !self.close_block(sink) {
                return Ok(false);
            }
            let name = call.get("name").and_then(Value::as_str).ok_or_else(|| {
                InferenceError::Other("Google function call has no name".to_string())
            })?;
            let args = call.get("args").cloned().unwrap_or_else(|| json!({}));
            if !args.is_object() {
                return Err(InferenceError::Other(
                    "Google function arguments must be an object".to_string(),
                ));
            }
            let id = call.get("id").and_then(Value::as_str).map_or_else(
                || -> Result<String> {
                    // Older models omit call ids. Include random bytes so two
                    // same-name calls in different turns cannot collide.
                    let mut bytes = [0; 16];
                    getrandom::fill(&mut bytes).map_err(|error| {
                        InferenceError::Other(format!("Cannot create Google tool call id: {error}"))
                    })?;
                    use core::fmt::Write as _;
                    let mut id = String::from("cupel_");
                    for byte in bytes {
                        let _ = write!(id, "{byte:02x}");
                    }
                    Ok(id)
                },
                |id| Ok(id.to_string()),
            )?;
            let index = self.output.content.len();
            let delta = args.to_string();
            self.output
                .content
                .push(AssistantContent::ToolCall(ToolCall {
                    id,
                    name: name.to_string(),
                    arguments: args,
                    thought_signature: signature.map(str::to_string),
                }));
            self.active = Some(index);
            return Ok(sink.toolcall_start(index) && sink.toolcall_delta(index, delta));
        }
        let text = part.get("text").and_then(Value::as_str);
        if text.is_none() && signature.is_none() {
            return Ok(true);
        }
        let thinking = part.get("thought").and_then(Value::as_bool) == Some(true);
        // A signature belongs to this exact part, including an empty final
        // text part. Never fold a signed part into earlier unsigned text.
        let can_append = may_continue
            && signature.is_none()
            && self
                .active
                .is_some_and(|index| match &self.output.content[index] {
                    AssistantContent::Text(text) => !thinking && text.text_signature.is_none(),
                    AssistantContent::Thinking(text) => {
                        thinking && text.thinking_signature.is_none()
                    }
                    AssistantContent::ToolCall(_) => false,
                });
        if !can_append {
            if !self.close_block(sink) {
                return Ok(false);
            }
            let index = self.output.content.len();
            self.output.content.push(if thinking {
                AssistantContent::Thinking(ThinkingContent {
                    thinking: String::new(),
                    thinking_signature: None,
                    redacted: None,
                })
            } else {
                AssistantContent::Text(TextContent::plain(""))
            });
            self.active = Some(index);
            if !(if thinking {
                sink.thinking_start(index)
            } else {
                sink.text_start(index)
            }) {
                return Ok(false);
            }
        }
        let Some(index) = self.active else {
            return Ok(false);
        };
        let delta = text.unwrap_or("");
        match &mut self.output.content[index] {
            AssistantContent::Text(text) => {
                text.text.push_str(delta);
                text.text_signature = signature.map(str::to_string);
            }
            AssistantContent::Thinking(text) => {
                text.thinking.push_str(delta);
                text.thinking_signature = signature.map(str::to_string);
            }
            AssistantContent::ToolCall(_) => {}
        }
        Ok(delta.is_empty()
            || if thinking {
                sink.thinking_delta(index, delta.to_string())
            } else {
                sink.text_delta(index, delta.to_string())
            })
    }

    fn close_block(&mut self, sink: &EventSink) -> bool {
        let Some(index) = self.active.take() else {
            return true;
        };
        match &self.output.content[index] {
            AssistantContent::Text(text) => sink.text_end(index, text.text.clone()),
            AssistantContent::Thinking(text) => sink.thinking_end(index, text.thinking.clone()),
            AssistantContent::ToolCall(call) => sink.toolcall_end(index, call.clone()),
        }
    }
}

#[cfg(test)]
mod tests;
