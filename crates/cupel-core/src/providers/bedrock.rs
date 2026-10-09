//! AWS Bedrock `ConverseStream` provider.
//!
//! Unlike Anthropic/`OpenAI`, Bedrock does not speak SSE:
//! responses use AWS's binary event-stream encoding, and requests
//! must be SigV4-signed. Rather than hand-rolling
//! either (both are security-sensitive and finicky), we use the official
//! `aws-sdk-bedrockruntime` crate. The SDK also gives us the standard
//! credential chain (env vars, `~/.aws/credentials`, SSO, IMDS) for free.
//!
//! Intentional first-iteration simplifications (documented, not
//! forgotten): no bearer-token auth, no HTTP proxy override, no GovCloud
//! schema workaround, no 1h cache TTL (the Rust SDK exposes cache points but
//! not their TTL yet).

use aws_sdk_bedrockruntime::types as bedrock;
use aws_smithy_types::Document;
use base64::Engine as _;
use serde_json::{Value, json};

use crate::{
    error::{InferenceError, Result},
    event_stream::{AssistantMessageStream, EventSink, assistant_message_channel},
    json_util::parse_streaming_json,
    model::calculate_cost,
    options_util::{adjust_max_tokens_for_thinking, clamp_max_tokens_to_context},
    provider::Provider,
    providers::{
        CONTENT_FILTER_MESSAGE, error_message, finish_output, new_output_message, with_cancel,
    },
    transform::transform_messages,
    types::{
        Api, AssistantContent, AssistantMessage, CacheRetention, Context, Message, Model,
        StopReason, StreamOptions, TextContent, ThinkingContent, ThinkingLevel, ToolResultContent,
        UserContent, UserContentBody,
    },
};

/// Bedrock rejects empty text blocks, so substitute this placeholder.
const EMPTY_TEXT_PLACEHOLDER: &str = "<empty>";

pub struct BedrockProvider;

impl BedrockProvider {
    #[must_use]
    pub fn new() -> Self {
        Self
    }
}

impl Default for BedrockProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl Provider for BedrockProvider {
    fn api(&self) -> &str {
        Api::BEDROCK_CONVERSE_STREAM
    }

    fn stream(
        &self,
        model: &Model,
        context: Context,
        options: StreamOptions,
    ) -> AssistantMessageStream {
        let (stream, sink) = assistant_message_channel();
        let model = model.clone();

        tokio::spawn(async move {
            if let Err(err) = run(&model, &context, &options, &sink).await {
                tracing::warn!(error = %err, "provider request failed");
                let msg = error_message(&model, &err);
                let _ = sink.error(msg.stop_reason, msg);
            }
        });

        stream
    }
}

// Bedrock hosts many model families behind one API; Claude-specific features
// (thinking, prompt caching, signatures) must only be sent to Claude models.
// The model id may be a bare id, an inference-profile id, or an ARN, so we
// match against both id and display name, in several normalizations.

fn match_candidates(model: &Model) -> Vec<String> {
    [model.id.as_str(), model.name.as_str()]
        .iter()
        .flat_map(|value| {
            let lower = value.to_lowercase();
            let dashed: String = lower
                .chars()
                .map(|c| {
                    if matches!(c, ' ' | '_' | '.' | ':') {
                        '-'
                    } else {
                        c
                    }
                })
                .collect();
            [lower, dashed]
        })
        .collect()
}

fn is_claude_model(model: &Model) -> bool {
    let id = model.id.to_lowercase();
    let name = model.name.to_lowercase();
    id.contains("anthropic.claude") || id.contains("anthropic/claude") || name.contains("claude")
}

/// Opus 4.6+/Sonnet 4.6+/Fable 5 use adaptive thinking (effort) instead of
/// token budgets.
fn supports_adaptive_thinking(model: &Model) -> bool {
    match_candidates(model).iter().any(|s| {
        s.contains("opus-4-6")
            || s.contains("opus-4-7")
            || s.contains("opus-4-8")
            || s.contains("sonnet-4-6")
            || s.contains("sonnet-5")
            || s.contains("fable-5")
    })
}

/// The xhigh effort arrived with Opus 4.7; the 4.6 models stop at high.
/// "sonnet-5" also matches Sonnet 5.5.
fn supports_native_xhigh(model: &Model) -> bool {
    match_candidates(model).iter().any(|s| {
        s.contains("opus-4-7")
            || s.contains("opus-4-8")
            || s.contains("fable-5")
            || s.contains("sonnet-5")
    })
}

/// Prompt caching is only available on newer Claude models. Application
/// inference profiles hide the model name in the ARN. There the model's
/// display name (user-controlled) is the only signal.
fn supports_prompt_caching(model: &Model) -> bool {
    let candidates = match_candidates(model);
    if !candidates.iter().any(|s| s.contains("claude")) {
        return false;
    }
    candidates.iter().any(|s| {
        s.contains("fable-5")
            || s.contains("sonnet-5")
            || s.contains("-4-") // any Claude 4.x
            || s.contains("claude-3-7-sonnet")
            || s.contains("claude-3-5-haiku")
    })
}

#[tracing::instrument(name = "bedrock_request", skip_all, fields(model = %model.id, provider = %model.provider.as_str()))]
async fn run(
    model: &Model,
    context: &Context,
    options: &StreamOptions,
    sink: &EventSink,
) -> Result<()> {
    let client = build_client(model, options).await;
    let cache_retention = options.cache_retention.unwrap_or(CacheRetention::Short);

    // max_tokens / thinking budget
    let is_claude = is_claude_model(model);
    let mut max_tokens: Option<u64> = options.max_tokens.or_else(|| {
        // Claude models get an explicit cap; other families use their own
        // defaults when we omit the field.
        is_claude.then_some(model.max_tokens)
    });
    let mut thinking_budget_override: Option<u64> = None;

    if let Some(level) = options.reasoning
        && is_claude
        && !supports_adaptive_thinking(model)
    {
        let adjusted = adjust_max_tokens_for_thinking(
            options.max_tokens,
            model.max_tokens,
            level,
            options.thinking_budgets,
        );
        let clamped = clamp_max_tokens_to_context(model, context, adjusted.max_tokens);
        max_tokens = Some(clamped);
        thinking_budget_override = Some(adjusted.thinking_budget.min(clamped.saturating_sub(1024)));
    }

    // Build the ConverseStream request
    let mut request = client
        .converse_stream()
        .model_id(&model.id)
        .set_messages(Some(convert_messages(context, model, cache_retention)?))
        .set_system(build_system_prompt(context, model, cache_retention))
        .set_tool_config(convert_tool_config(context)?);

    let mut inference = bedrock::InferenceConfiguration::builder();
    if let Some(max_tokens) = max_tokens {
        inference = inference.max_tokens(i32::try_from(max_tokens).unwrap_or(i32::MAX));
    }
    if let Some(temperature) = options.temperature {
        // Temperature and extended thinking are mutually exclusive on Claude.
        if options.reasoning.is_none() {
            inference = inference.temperature(temperature as f32);
        }
    }
    request = request.inference_config(inference.build());

    if let Some(fields) =
        build_additional_model_request_fields(model, options, thinking_budget_override)
    {
        request = request.additional_model_request_fields(json_to_document(&fields));
    }

    // Send + stream
    let response = with_cancel(options, request.send())
        .await?
        .map_err(|e| InferenceError::Other(format_sdk_error(&e)))?;

    let stream = futures_util::stream::try_unfold(response.stream, |mut stream| async move {
        let item = stream
            .recv()
            .await
            .map_err(|e| InferenceError::Other(format_sdk_error(&e)))?;
        Ok(item.map(|item| (item, stream)))
    });
    consume_stream(model, options, sink, stream).await
}

/// Translate SDK events independently of the network and credential chain.
async fn consume_stream(
    model: &Model,
    options: &StreamOptions,
    sink: &EventSink,
    stream: impl futures_core::Stream<Item = Result<bedrock::ConverseStreamOutput>>,
) -> Result<()> {
    use futures_util::{StreamExt as _, pin_mut};

    pin_mut!(stream);
    let mut output = new_output_message(model);

    // Bedrock indexes content blocks like Anthropic does; track the mapping
    // to our positions plus tool-call JSON scratch buffers.
    struct BlockState {
        bedrock_index: i32,
        kind: BlockKind,
    }
    enum BlockKind {
        Text,
        Thinking,
        ToolCall { partial_json: String },
    }
    let mut blocks: Vec<BlockState> = Vec::new();
    let find = |blocks: &[BlockState], idx: i32| blocks.iter().position(|b| b.bedrock_index == idx);
    let mut saw_message_stop = false;

    loop {
        let item = with_cancel(options, stream.next()).await?;
        let Some(item) = item else { break };

        match item? {
            bedrock::ConverseStreamOutput::MessageStart(start) => {
                if start.role != bedrock::ConversationRole::Assistant {
                    return Err(InferenceError::Other(
                        "Expected assistant message start but got a different role".to_string(),
                    ));
                }
                if !sink.start() {
                    return Ok(());
                }
            }

            bedrock::ConverseStreamOutput::ContentBlockStart(start) => {
                // Only tool_use blocks announce themselves with a start event;
                // text and reasoning blocks appear directly as deltas.
                let index = start.content_block_index;
                if let Some(bedrock::ContentBlockStart::ToolUse(tool_use)) = start.start {
                    output
                        .content
                        .push(AssistantContent::ToolCall(crate::types::ToolCall {
                            id: tool_use.tool_use_id,
                            name: tool_use.name,
                            arguments: json!({}),
                        }));
                    blocks.push(BlockState {
                        bedrock_index: index,
                        kind: BlockKind::ToolCall {
                            partial_json: String::new(),
                        },
                    });
                    if !sink.toolcall_start(output.content.len() - 1) {
                        return Ok(());
                    }
                }
            }

            bedrock::ConverseStreamOutput::ContentBlockDelta(event) => {
                let index = event.content_block_index;
                let Some(delta) = event.delta else { continue };
                match delta {
                    bedrock::ContentBlockDelta::Text(text) => {
                        // Text blocks get no start event: create lazily.
                        let pos = match find(&blocks, index) {
                            Some(pos) => pos,
                            None => {
                                output
                                    .content
                                    .push(AssistantContent::Text(TextContent::plain("")));
                                blocks.push(BlockState {
                                    bedrock_index: index,
                                    kind: BlockKind::Text,
                                });
                                let pos = output.content.len() - 1;
                                if !sink.text_start(pos) {
                                    return Ok(());
                                }
                                pos
                            }
                        };
                        if let Some(AssistantContent::Text(block)) = output.content.get_mut(pos) {
                            block.text.push_str(&text);
                            if !sink.text_delta(pos, text) {
                                return Ok(());
                            }
                        }
                    }
                    bedrock::ContentBlockDelta::ToolUse(tool_delta) => {
                        if let Some(pos) = find(&blocks, index)
                            && let Some(BlockState {
                                kind: BlockKind::ToolCall { partial_json },
                                ..
                            }) = blocks.get_mut(pos)
                        {
                            partial_json.push_str(&tool_delta.input);
                            let parsed = parse_streaming_json(partial_json);
                            if let Some(AssistantContent::ToolCall(tc)) =
                                output.content.get_mut(pos)
                            {
                                tc.arguments = parsed;
                            }
                            if !sink.toolcall_delta(pos, tool_delta.input) {
                                return Ok(());
                            }
                        }
                    }
                    bedrock::ContentBlockDelta::ReasoningContent(reasoning) => {
                        // Reasoning blocks also appear without a start event.
                        let pos = match find(&blocks, index) {
                            Some(pos) => pos,
                            None => {
                                output
                                    .content
                                    .push(AssistantContent::Thinking(ThinkingContent {
                                        thinking: String::new(),
                                        thinking_signature: Some(String::new()),
                                        redacted: None,
                                    }));
                                blocks.push(BlockState {
                                    bedrock_index: index,
                                    kind: BlockKind::Thinking,
                                });
                                let pos = output.content.len() - 1;
                                if !sink.thinking_start(pos) {
                                    return Ok(());
                                }
                                pos
                            }
                        };
                        if let Some(AssistantContent::Thinking(block)) = output.content.get_mut(pos)
                        {
                            match reasoning {
                                bedrock::ReasoningContentBlockDelta::Text(text) => {
                                    block.thinking.push_str(&text);
                                    if !sink.thinking_delta(pos, text) {
                                        return Ok(());
                                    }
                                }
                                bedrock::ReasoningContentBlockDelta::Signature(signature) => {
                                    block
                                        .thinking_signature
                                        .get_or_insert_with(String::new)
                                        .push_str(&signature);
                                }
                                _ => {}
                            }
                        }
                    }
                    _ => {}
                }
            }

            bedrock::ConverseStreamOutput::ContentBlockStop(stop) => {
                let Some(pos) = find(&blocks, stop.content_block_index) else {
                    continue;
                };
                match &blocks[pos].kind {
                    BlockKind::Text => {
                        if let Some(AssistantContent::Text(block)) = output.content.get(pos)
                            && !sink.text_end(pos, block.text.clone())
                        {
                            return Ok(());
                        }
                    }
                    BlockKind::Thinking => {
                        if let Some(AssistantContent::Thinking(block)) = output.content.get(pos)
                            && !sink.thinking_end(pos, block.thinking.clone())
                        {
                            return Ok(());
                        }
                    }
                    BlockKind::ToolCall { partial_json } => {
                        let parsed = parse_streaming_json(partial_json);
                        if let Some(AssistantContent::ToolCall(tc)) = output.content.get_mut(pos) {
                            tc.arguments = parsed;
                            if !sink.toolcall_end(pos, tc.clone()) {
                                return Ok(());
                            }
                        }
                    }
                }
            }

            bedrock::ConverseStreamOutput::MessageStop(stop) => {
                saw_message_stop = true;
                let (reason, error) = map_stop_reason(&stop.stop_reason);
                output.stop_reason = reason;
                output.error_message = error;
            }

            bedrock::ConverseStreamOutput::Metadata(metadata) => {
                if let Some(usage) = metadata.usage {
                    output.usage.input = u64::try_from(usage.input_tokens).unwrap_or(0);
                    output.usage.output = u64::try_from(usage.output_tokens).unwrap_or(0);
                    output.usage.cache_read = usage
                        .cache_read_input_tokens
                        .and_then(|v| u64::try_from(v).ok())
                        .unwrap_or(0);
                    output.usage.cache_write = usage
                        .cache_write_input_tokens
                        .and_then(|v| u64::try_from(v).ok())
                        .unwrap_or(0);
                    output.usage.total_tokens = u64::try_from(usage.total_tokens)
                        .unwrap_or(output.usage.input + output.usage.output);
                    calculate_cost(model, &mut output.usage);
                }
            }

            _ => {}
        }
    }

    if !saw_message_stop {
        return Err(InferenceError::Other(
            "Bedrock stream ended before MessageStop".to_string(),
        ));
    }

    finish_output(output, sink);
    Ok(())
}

/// Region resolution, in precedence order:
/// ARN-embedded region > env override > SDK default chain > `us-east-1`.
async fn build_client(model: &Model, options: &StreamOptions) -> aws_sdk_bedrockruntime::Client {
    // 1. When the model id is an inference-profile ARN
    //    (`arn:aws:bedrock:REGION:...`), the region is baked into it.
    let arn_region = model.id.strip_prefix("arn:").and_then(|rest| {
        // Skip the partition segment (aws / aws-cn / aws-us-gov).
        let mut parts = rest.split(':');
        let _partition = parts.next()?;
        let service = parts.next()?;
        let region = parts.next()?;
        (service == "bedrock" && !region.is_empty()).then(|| region.to_string())
    });

    // 2. Explicit override via options.env, letting embedders inject config
    //    without touching process-wide environment variables.
    let env_region = options.env.as_ref().and_then(|env| {
        env.get("AWS_REGION")
            .or_else(|| env.get("AWS_DEFAULT_REGION"))
            .cloned()
    });

    let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest());
    if let Some(region) = arn_region.or(env_region) {
        loader = loader.region(aws_config::Region::new(region));
    } else {
        // 3./4. Default chain (env, profile, IMDS) with us-east-1 fallback.
        loader = loader.region(
            aws_config::meta::region::RegionProviderChain::default_provider().or_else("us-east-1"),
        );
    }
    if let Some(profile) = options.env.as_ref().and_then(|env| env.get("AWS_PROFILE")) {
        loader = loader.profile_name(profile);
    }

    let sdk_config = loader.load().await;
    let mut builder = aws_sdk_bedrockruntime::config::Builder::from(&sdk_config);

    // Custom endpoints (VPC endpoints, proxies) are configured via the
    // model's base_url. Standard bedrock-runtime hosts are left to the SDK
    // so region config stays authoritative.
    if !model.base_url.is_empty() && !is_standard_bedrock_endpoint(&model.base_url) {
        builder = builder.endpoint_url(&model.base_url);
    }

    aws_sdk_bedrockruntime::Client::from_conf(builder.build())
}

fn is_standard_bedrock_endpoint(base_url: &str) -> bool {
    let host = base_url
        .trim_start_matches("https://")
        .trim_start_matches("http://");
    let host = host.split('/').next().unwrap_or(host).to_lowercase();
    host.starts_with("bedrock-runtime")
        && (host.ends_with(".amazonaws.com") || host.ends_with(".amazonaws.com.cn"))
}

fn format_sdk_error<E, R>(err: &aws_sdk_bedrockruntime::error::SdkError<E, R>) -> String
where
    E: core::fmt::Display,
    R: core::fmt::Debug,
{
    use aws_sdk_bedrockruntime::error::SdkError;
    match err {
        // The service error carries the actual API message (validation,
        // throttling, ...); the generic Display for SdkError hides it.
        SdkError::ServiceError(service_err) => format!("Bedrock error: {}", service_err.err()),
        other => format!("Bedrock transport error: {other}"),
    }
}

fn map_stop_reason(reason: &bedrock::StopReason) -> (StopReason, Option<String>) {
    // Matching on the string form keeps us forward-compatible: the enum is
    // non-exhaustive and AWS adds variants (e.g. model_context_window_exceeded).
    match reason.as_str() {
        "end_turn" | "stop_sequence" => (StopReason::Stop, None),
        "max_tokens" | "model_context_window_exceeded" => (StopReason::Length, None),
        "tool_use" => (StopReason::ToolUse, None),
        "content_filtered" => (StopReason::Error, Some(CONTENT_FILTER_MESSAGE.to_string())),
        "guardrail_intervened" => (
            StopReason::Error,
            Some(
                "The provider's guardrail stopped the response. Partial output may be incomplete."
                    .to_string(),
            ),
        ),
        other => (
            StopReason::Error,
            Some(format!("Provider stop_reason: {other}")),
        ),
    }
}

fn cache_point() -> Result<bedrock::ContentBlock> {
    Ok(bedrock::ContentBlock::CachePoint(
        bedrock::CachePointBlock::builder()
            .r#type(bedrock::CachePointType::Default)
            .build()
            .map_err(|e| InferenceError::Other(e.to_string()))?,
    ))
}

fn build_system_prompt(
    context: &Context,
    model: &Model,
    cache_retention: CacheRetention,
) -> Option<Vec<bedrock::SystemContentBlock>> {
    let prompt = context.system_prompt.as_ref()?;
    let mut blocks = vec![bedrock::SystemContentBlock::Text(prompt.clone())];
    if cache_retention != CacheRetention::None
        && supports_prompt_caching(model)
        && let Ok(point) = bedrock::CachePointBlock::builder()
            .r#type(bedrock::CachePointType::Default)
            .build()
    {
        blocks.push(bedrock::SystemContentBlock::CachePoint(point));
    }
    Some(blocks)
}

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

/// A text block, or `None` when the text is blank (Bedrock rejects blanks).
fn non_blank_text_block(text: &str) -> Option<bedrock::ContentBlock> {
    (!text.trim().is_empty()).then(|| bedrock::ContentBlock::Text(text.to_string()))
}

fn required_text_block(text: &str) -> bedrock::ContentBlock {
    non_blank_text_block(text)
        .unwrap_or_else(|| bedrock::ContentBlock::Text(EMPTY_TEXT_PLACEHOLDER.to_string()))
}

fn image_block(mime_type: &str, data: &str) -> Result<bedrock::ContentBlock> {
    let format = match mime_type {
        "image/jpeg" | "image/jpg" => bedrock::ImageFormat::Jpeg,
        "image/png" => bedrock::ImageFormat::Png,
        "image/gif" => bedrock::ImageFormat::Gif,
        "image/webp" => bedrock::ImageFormat::Webp,
        other => {
            return Err(InferenceError::Other(format!(
                "Unknown image type: {other}"
            )));
        }
    };
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|e| InferenceError::Other(format!("Invalid base64 image data: {e}")))?;
    let image = bedrock::ImageBlock::builder()
        .format(format)
        .source(bedrock::ImageSource::Bytes(aws_smithy_types::Blob::new(
            bytes,
        )))
        .build()
        .map_err(|e| InferenceError::Other(e.to_string()))?;
    Ok(bedrock::ContentBlock::Image(image))
}

fn tool_result_content(
    content: &[ToolResultContent],
) -> Result<Vec<bedrock::ToolResultContentBlock>> {
    let mut result = Vec::new();
    for block in content {
        match block {
            ToolResultContent::Text(text) => {
                if !text.text.trim().is_empty() {
                    result.push(bedrock::ToolResultContentBlock::Text(text.text.clone()));
                }
            }
            ToolResultContent::Image(image) => {
                // Reuse image_block's validation, then unwrap the enum.
                if let bedrock::ContentBlock::Image(img) =
                    image_block(&image.mime_type, &image.data)?
                {
                    result.push(bedrock::ToolResultContentBlock::Image(img));
                }
            }
        }
    }
    if result.is_empty() {
        result.push(bedrock::ToolResultContentBlock::Text(
            EMPTY_TEXT_PLACEHOLDER.to_string(),
        ));
    }
    Ok(result)
}

fn convert_messages(
    context: &Context,
    model: &Model,
    cache_retention: CacheRetention,
) -> Result<Vec<bedrock::Message>> {
    let transformed = transform_messages(&context.messages, model, Some(normalize_tool_call_id));
    let supports_signature = is_claude_model(model);
    let mut result: Vec<bedrock::Message> = Vec::new();

    let build_message = |role: bedrock::ConversationRole, content: Vec<bedrock::ContentBlock>| {
        bedrock::Message::builder()
            .role(role)
            .set_content(Some(content))
            .build()
            .map_err(|e| InferenceError::Other(e.to_string()))
    };

    let mut i = 0;
    while i < transformed.len() {
        match &transformed[i] {
            Message::User(user) => {
                let mut content: Vec<bedrock::ContentBlock> = Vec::new();
                match &user.content {
                    UserContentBody::Text(text) => content.push(required_text_block(text)),
                    UserContentBody::Blocks(user_blocks) => {
                        for block in user_blocks {
                            match block {
                                UserContent::Text(text) => {
                                    if let Some(b) = non_blank_text_block(&text.text) {
                                        content.push(b);
                                    }
                                }
                                UserContent::Image(image) => {
                                    content.push(image_block(&image.mime_type, &image.data)?);
                                }
                            }
                        }
                        if content.is_empty() {
                            content.push(required_text_block(""));
                        }
                    }
                }
                result.push(build_message(bedrock::ConversationRole::User, content)?);
                i += 1;
            }

            Message::Assistant(assistant) => {
                let mut content: Vec<bedrock::ContentBlock> = Vec::new();
                for block in &assistant.content {
                    match block {
                        AssistantContent::Text(text) => {
                            if let Some(b) = non_blank_text_block(&text.text) {
                                content.push(b);
                            }
                        }
                        AssistantContent::ToolCall(tool_call) => {
                            let tool_use = bedrock::ToolUseBlock::builder()
                                .tool_use_id(&tool_call.id)
                                .name(&tool_call.name)
                                .input(json_to_document(&tool_call.arguments))
                                .build()
                                .map_err(|e| InferenceError::Other(e.to_string()))?;
                            content.push(bedrock::ContentBlock::ToolUse(tool_use));
                        }
                        AssistantContent::Thinking(thinking) => {
                            if thinking.thinking.trim().is_empty() {
                                continue;
                            }
                            let signature = thinking
                                .thinking_signature
                                .as_deref()
                                .filter(|s| !s.trim().is_empty());
                            if supports_signature {
                                match signature {
                                    // Signatures arrive after thinking deltas;
                                    // an aborted stream leaves them empty and
                                    // Bedrock rejects the replayed block. Fall
                                    // back to plain text (same as Anthropic).
                                    None => content.push(bedrock::ContentBlock::Text(
                                        thinking.thinking.clone(),
                                    )),
                                    Some(signature) => {
                                        let text_block = bedrock::ReasoningTextBlock::builder()
                                            .text(&thinking.thinking)
                                            .signature(signature)
                                            .build()
                                            .map_err(|e| InferenceError::Other(e.to_string()))?;
                                        content.push(bedrock::ContentBlock::ReasoningContent(
                                            bedrock::ReasoningContentBlock::ReasoningText(
                                                text_block,
                                            ),
                                        ));
                                    }
                                }
                            } else {
                                // Non-Claude models reject the signature field.
                                let text_block = bedrock::ReasoningTextBlock::builder()
                                    .text(&thinking.thinking)
                                    .build()
                                    .map_err(|e| InferenceError::Other(e.to_string()))?;
                                content.push(bedrock::ContentBlock::ReasoningContent(
                                    bedrock::ReasoningContentBlock::ReasoningText(text_block),
                                ));
                            }
                        }
                    }
                }
                // Skip messages whose content was entirely filtered out
                // (Bedrock rejects empty content arrays).
                if !content.is_empty() {
                    result.push(build_message(
                        bedrock::ConversationRole::Assistant,
                        content,
                    )?);
                }
                i += 1;
            }

            Message::ToolResult(_) => {
                // Bedrock requires all tool results for one assistant turn in
                // a single user message; collect the consecutive run.
                let mut content: Vec<bedrock::ContentBlock> = Vec::new();
                while let Some(Message::ToolResult(tool_result)) = transformed.get(i) {
                    let block = bedrock::ToolResultBlock::builder()
                        .tool_use_id(&tool_result.tool_call_id)
                        .set_content(Some(tool_result_content(&tool_result.content)?))
                        .status(if tool_result.is_error {
                            bedrock::ToolResultStatus::Error
                        } else {
                            bedrock::ToolResultStatus::Success
                        })
                        .build()
                        .map_err(|e| InferenceError::Other(e.to_string()))?;
                    content.push(bedrock::ContentBlock::ToolResult(block));
                    i += 1;
                }
                result.push(build_message(bedrock::ConversationRole::User, content)?);
            }
        }
    }

    // Cache the conversation prefix by placing a cache point at the end of
    // the last user message (same strategy as the Anthropic provider).
    if cache_retention != CacheRetention::None
        && supports_prompt_caching(model)
        && let Some(last) = result.last_mut()
        && last.role == bedrock::ConversationRole::User
    {
        let mut content = last.content.clone();
        content.push(cache_point()?);
        *last = bedrock::Message::builder()
            .role(bedrock::ConversationRole::User)
            .set_content(Some(content))
            .build()
            .map_err(|e| InferenceError::Other(e.to_string()))?;
    }

    Ok(result)
}

fn convert_tool_config(context: &Context) -> Result<Option<bedrock::ToolConfiguration>> {
    let Some(tools) = &context.tools else {
        return Ok(None);
    };
    if tools.is_empty() {
        return Ok(None);
    }

    let bedrock_tools: Vec<bedrock::Tool> = tools
        .iter()
        .map(|tool| {
            let spec = bedrock::ToolSpecification::builder()
                .name(&tool.name)
                .description(&tool.description)
                .input_schema(bedrock::ToolInputSchema::Json(json_to_document(
                    &tool.parameters,
                )))
                .build()
                .map_err(|e| InferenceError::Other(e.to_string()))?;
            Ok(bedrock::Tool::ToolSpec(spec))
        })
        .collect::<Result<_>>()?;

    Ok(Some(
        bedrock::ToolConfiguration::builder()
            .set_tools(Some(bedrock_tools))
            .build()
            .map_err(|e| InferenceError::Other(e.to_string()))?,
    ))
}

/// Thinking configuration travels in `additionalModelRequestFields`, a
/// free-form JSON escape hatch for model-family-specific parameters that the
/// Converse schema doesn't cover.
fn build_additional_model_request_fields(
    model: &Model,
    options: &StreamOptions,
    thinking_budget_override: Option<u64>,
) -> Option<Value> {
    if !model.reasoning || !is_claude_model(model) {
        return None;
    }
    let Some(level) = options.reasoning else {
        // Match Anthropic's off convention: no entry explicitly disables
        // thinking, a named entry sends that type alone (Sonnet 5.5:
        // `between_tools`), and null means thinking cannot be switched off.
        let off = model.thinking_level_map.as_ref().and_then(|m| m.get("off"));
        return match off {
            None => Some(json!({"thinking": {"type": "disabled"}})),
            Some(Some(off_type)) => Some(json!({"thinking": {"type": off_type}})),
            Some(None) => None,
        };
    };

    if supports_adaptive_thinking(model) {
        return Some(json!({
            "thinking": {"type": "adaptive", "display": "summarized"},
            "output_config": {"effort": map_thinking_level_to_effort(model, level)},
        }));
    }

    // Budget-based thinking for older Claude models.
    let default_budget = match level {
        ThinkingLevel::Minimal => 1024,
        ThinkingLevel::Low => 2048,
        ThinkingLevel::Medium => 8192,
        // Claude budget models don't support xhigh; clamp to high.
        ThinkingLevel::High | ThinkingLevel::XHigh | ThinkingLevel::Max => 16384,
    };
    let custom_budget = options.thinking_budgets.and_then(|b| match level {
        ThinkingLevel::Minimal => b.minimal,
        ThinkingLevel::Low => b.low,
        ThinkingLevel::Medium => b.medium,
        ThinkingLevel::High | ThinkingLevel::XHigh | ThinkingLevel::Max => b.high,
    });
    let budget = thinking_budget_override
        .or(custom_budget)
        .unwrap_or(default_budget);

    Some(json!({
        "thinking": {
            "type": "enabled",
            "budget_tokens": budget,
            "display": "summarized",
        },
        // Interleaved thinking (thinking between tool calls) is opt-in via
        // beta flag on budget models; adaptive models have it built in.
        "anthropic_beta": ["interleaved-thinking-2025-05-14"],
    }))
}

fn map_thinking_level_to_effort(model: &Model, level: ThinkingLevel) -> String {
    if level == ThinkingLevel::XHigh && supports_native_xhigh(model) {
        return "xhigh".to_string();
    }
    let key = match level {
        ThinkingLevel::Minimal => "minimal",
        ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        ThinkingLevel::High => "high",
        ThinkingLevel::XHigh => "xhigh",
        ThinkingLevel::Max => "max",
    };
    if let Some(Some(mapped)) = model.thinking_level_map.as_ref().and_then(|m| m.get(key)) {
        return mapped.clone();
    }
    match level {
        ThinkingLevel::Minimal | ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        // Only reached without native xhigh (see supports_native_xhigh).
        ThinkingLevel::High | ThinkingLevel::XHigh => "high",
        // Every adaptive model has max, the 4.6 ones included.
        ThinkingLevel::Max => "max",
    }
    .to_string()
}

/// Convert `serde_json::Value` into the AWS SDK's `Document` type. The two
/// are structurally identical; the SDK just refuses to depend on serde_json.
fn json_to_document(value: &Value) -> Document {
    match value {
        Value::Null => Document::Null,
        Value::Bool(b) => Document::Bool(*b),
        Value::Number(n) => {
            if let Some(u) = n.as_u64() {
                Document::Number(aws_smithy_types::Number::PosInt(u))
            } else if let Some(i) = n.as_i64() {
                Document::Number(aws_smithy_types::Number::NegInt(i))
            } else {
                Document::Number(aws_smithy_types::Number::Float(n.as_f64().unwrap_or(0.0)))
            }
        }
        Value::String(s) => Document::String(s.clone()),
        Value::Array(items) => Document::Array(items.iter().map(json_to_document).collect()),
        Value::Object(map) => Document::Object(
            map.iter()
                .map(|(k, v)| (k.clone(), json_to_document(v)))
                .collect(),
        ),
    }
}

#[cfg(test)]
mod tests {
    use aws_sdk_bedrockruntime::types as bedrock;
    use futures_util::StreamExt as _;
    use serde_json::{Value, json};

    use crate::error::{InferenceError, Result};
    use crate::event_stream::assistant_message_channel;
    use crate::providers::bedrock::{
        build_additional_model_request_fields, consume_stream, format_sdk_error,
    };
    use crate::providers::error_message;
    use crate::retry::is_retryable_assistant_error;
    use crate::types::{AssistantMessageEvent, Model, StopReason, StreamOptions, ThinkingLevel};

    /// A row from the shipped catalog, not a hand-built fixture: these
    /// tests break if catalog.json and this provider drift apart.
    fn catalog_model(id: &str) -> Model {
        crate::catalog::builtin_models()
            .into_iter()
            .find(|m| m.id == id)
            .expect("model in catalog")
    }

    /// The `additionalModelRequestFields` for one thinking level.
    fn fields(model: &Model, reasoning: Option<ThinkingLevel>) -> Option<Value> {
        let options = StreamOptions {
            reasoning,
            ..StreamOptions::default()
        };
        build_additional_model_request_fields(model, &options, None)
    }

    fn message_start() -> bedrock::ConverseStreamOutput {
        bedrock::ConverseStreamOutput::MessageStart(
            bedrock::MessageStartEvent::builder()
                .role(bedrock::ConversationRole::Assistant)
                .build()
                .unwrap(),
        )
    }

    fn text_delta() -> bedrock::ConverseStreamOutput {
        bedrock::ConverseStreamOutput::ContentBlockDelta(
            bedrock::ContentBlockDeltaEvent::builder()
                .content_block_index(0)
                .delta(bedrock::ContentBlockDelta::Text("partial output".into()))
                .build()
                .unwrap(),
        )
    }

    async fn collect_stream(
        events: Vec<bedrock::ConverseStreamOutput>,
    ) -> (Result<()>, Vec<AssistantMessageEvent>) {
        let model = catalog_model("global.anthropic.claude-sonnet-5-5");
        let (stream, sink) = assistant_message_channel();
        let result = consume_stream(
            &model,
            &StreamOptions::default(),
            &sink,
            futures_util::stream::iter(events.into_iter().map(Ok)),
        )
        .await;
        drop(sink);
        (result, stream.collect().await)
    }

    #[tokio::test]
    async fn streams_without_message_stop_are_retryable_errors() {
        for events in [
            Vec::new(),
            vec![message_start()],
            vec![message_start(), text_delta()],
        ] {
            let (result, emitted) = collect_stream(events).await;
            let error = result.expect_err("a stream without MessageStop must not succeed");
            assert_eq!(error.to_string(), "Bedrock stream ended before MessageStop");
            assert!(
                !emitted
                    .iter()
                    .any(|e| matches!(e, AssistantMessageEvent::Done { .. }))
            );
            let model = catalog_model("global.anthropic.claude-sonnet-5-5");
            let message = error_message(&model, &error);
            assert!(is_retryable_assistant_error(&message));
        }
    }

    #[test]
    fn sdk_stream_errors_keep_exception_details_for_classification() {
        use aws_sdk_bedrockruntime::error::SdkError;
        use bedrock::error::{
            ConverseStreamOutputError, InternalServerException, ModelStreamErrorException,
            ThrottlingException, ValidationException,
        };

        let model = catalog_model("global.anthropic.claude-sonnet-5-5");
        for (service_error, retryable, overflow) in [
            (
                ConverseStreamOutputError::ThrottlingException(
                    ThrottlingException::builder()
                        .message("Too many tokens, please wait before trying again")
                        .build(),
                ),
                true,
                false,
            ),
            (
                ConverseStreamOutputError::InternalServerException(
                    InternalServerException::builder()
                        .message("opaque failure")
                        .build(),
                ),
                true,
                false,
            ),
            (
                ConverseStreamOutputError::ModelStreamErrorException(
                    ModelStreamErrorException::builder()
                        .message("opaque failure")
                        .build(),
                ),
                true,
                false,
            ),
            (
                ConverseStreamOutputError::ValidationException(
                    ValidationException::builder()
                        .message("Input is too long for requested model")
                        .build(),
                ),
                false,
                true,
            ),
        ] {
            let sdk_error = SdkError::service_error(service_error, ());
            assert_eq!(sdk_error.to_string(), "service error");
            let error = InferenceError::Other(format_sdk_error(&sdk_error));
            let message = error_message(&model, &error);
            assert_eq!(is_retryable_assistant_error(&message), retryable, "{error}");
            assert_eq!(
                crate::overflow::is_context_overflow(&message, 200_000),
                overflow,
                "{error}"
            );
        }
    }

    #[tokio::test]
    async fn terminal_stops_keep_output_and_metadata_after_message_stop() {
        let metadata = bedrock::ConverseStreamOutput::Metadata(
            bedrock::ConverseStreamMetadataEvent::builder()
                .usage(
                    bedrock::TokenUsage::builder()
                        .input_tokens(10)
                        .output_tokens(2)
                        .total_tokens(12)
                        .build()
                        .unwrap(),
                )
                .metrics(
                    bedrock::ConverseStreamMetrics::builder()
                        .latency_ms(1)
                        .build()
                        .unwrap(),
                )
                .build(),
        );
        for (vendor_reason, expected, explanation) in [
            (bedrock::StopReason::EndTurn, StopReason::Stop, None),
            (
                bedrock::StopReason::ContentFiltered,
                StopReason::Error,
                Some("content filter"),
            ),
            (
                bedrock::StopReason::GuardrailIntervened,
                StopReason::Error,
                Some("guardrail"),
            ),
        ] {
            let stop = bedrock::ConverseStreamOutput::MessageStop(
                bedrock::MessageStopEvent::builder()
                    .stop_reason(vendor_reason)
                    .build()
                    .unwrap(),
            );
            let (result, emitted) =
                collect_stream(vec![message_start(), text_delta(), stop, metadata.clone()]).await;
            result.expect("a mapped stop must not fail the worker and discard output");
            let (reason, message) = match emitted.last() {
                Some(AssistantMessageEvent::Done { reason, message }) => {
                    assert!(explanation.is_none());
                    (reason, message)
                }
                Some(AssistantMessageEvent::Error { reason, error }) => {
                    assert!(explanation.is_some());
                    assert!(!is_retryable_assistant_error(error));
                    (reason, error)
                }
                _ => panic!("expected a terminal event"),
            };
            assert_eq!(*reason, expected);
            assert_eq!(message.stop_reason, expected);
            if let Some(explanation) = explanation {
                assert!(
                    message
                        .error_message
                        .as_deref()
                        .is_some_and(|text| text.contains(explanation))
                );
            }
            assert_eq!(message.usage.total_tokens, 12);
            assert_eq!(
                message.content,
                vec![crate::types::AssistantContent::Text(
                    crate::types::TextContent::plain("partial output")
                )]
            );
        }
    }

    #[test]
    fn off_without_a_map_entry_explicitly_disables_thinking() {
        for id in [
            "us.anthropic.claude-sonnet-5",
            "us.anthropic.claude-sonnet-4-5-20250929-v1:0",
        ] {
            let model = catalog_model(id);
            assert!(
                model
                    .thinking_level_map
                    .as_ref()
                    .and_then(|map| map.get("off"))
                    .is_none()
            );
            assert_eq!(
                fields(&model, None),
                Some(json!({"thinking": {"type": "disabled"}})),
                "{id}"
            );
        }
    }

    #[test]
    fn sonnet55_off_sends_between_tools() {
        // Same rule as on the Anthropic API: `disabled` is a 400, the "off"
        // entry names the thinking type, and the type travels alone.
        let sonnet = catalog_model("global.anthropic.claude-sonnet-5-5");
        assert_eq!(
            fields(&sonnet, None),
            Some(json!({"thinking": {"type": "between_tools"}}))
        );
    }

    #[test]
    fn sonnet55_levels_reach_xhigh_and_max() {
        // Before, "sonnet-5" had no native xhigh and max fell back to high,
        // so both top levels quietly became high on Bedrock.
        let sonnet = catalog_model("global.anthropic.claude-sonnet-5-5");
        for (level, effort) in [
            (ThinkingLevel::Minimal, "low"),
            (ThinkingLevel::Low, "low"),
            (ThinkingLevel::Medium, "medium"),
            (ThinkingLevel::High, "high"),
            (ThinkingLevel::XHigh, "xhigh"),
            (ThinkingLevel::Max, "max"),
        ] {
            assert_eq!(
                fields(&sonnet, Some(level)),
                Some(json!({
                    "thinking": {"type": "adaptive", "display": "summarized"},
                    "output_config": {"effort": effort},
                })),
                "{level:?}"
            );
        }
    }

    #[test]
    fn fable5_and_51_use_adaptive_effort() {
        for id in [
            "us.anthropic.claude-fable-5",
            "us.anthropic.claude-fable-5-1",
        ] {
            let model = catalog_model(id);
            assert_eq!(fields(&model, None), None, "{id}");
            assert_eq!(
                fields(&model, Some(ThinkingLevel::XHigh)),
                Some(json!({
                    "thinking": {"type": "adaptive", "display": "summarized"},
                    "output_config": {"effort": "xhigh"},
                })),
                "{id}"
            );
        }
    }
}
