use crate::{
    ApiFamily, AssistantMessage, AssistantMessageEvent, ContentBlock, FinishReason,
    InferenceContext, InferenceRequest, Message, ModelSpec, ToolCallDelta, ToolDefinition,
    error::InferenceError,
    event::InferenceStream,
    provider::{InferenceProvider, ResolvedInferenceRequest},
};
use async_stream::stream;
use futures::StreamExt;
use reqwest::Client;
use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone)]
pub struct OpenAiCompatProvider {
    http: Client,
}

impl OpenAiCompatProvider {
    #[must_use]
    pub fn new() -> Self {
        Self {
            http: Client::new(),
        }
    }
}

impl Default for OpenAiCompatProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl InferenceProvider for OpenAiCompatProvider {
    fn stream(&self, request: ResolvedInferenceRequest) -> InferenceStream {
        let http = self.http.clone();

        Box::pin(stream! {
            let model = request.model;
            let inference_request = request.request;

            let Some(base_url) = model.base_url.clone() else {
                yield error_event(InferenceError::InvalidBaseUrl {
                    base_url: "<missing>".to_owned(),
                });
                return;
            };

            let Some(api_key) = inference_request.options.api_key.clone() else {
                yield error_event(InferenceError::MissingApiKey {
                    provider: model.provider.0.clone(),
                });
                return;
            };

            let payload = match OpenAiChatRequest::try_from_resolved(&model, &inference_request) {
                Ok(payload) => payload,
                Err(error) => {
                    yield error_event(error);
                    return;
                }
            };

            let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));

            let response = match http
                .post(url)
                .bearer_auth(api_key.expose_secret())
                .json(&payload)
                .send()
                .await
            {
                Ok(response) => response,
                Err(error) => {
                    yield error_event(InferenceError::RequestFailed {
                        message: error.to_string(),
                    });
                    return;
                }
            };

            if !response.status().is_success() {
                let status = response.status().as_u16();
                let body = response.text().await.unwrap_or_default();

                yield error_event(InferenceError::ProviderHttpStatus { status, body });
                return;
            }

            let mut state = AssistantState::default();
            yield AssistantMessageEvent::Start { message: state.message.clone(), };

            let mut sse = SseDecoder::default();
            let mut byte_stream = response.bytes_stream();

            while let Some(next) = byte_stream.next().await {
                let chunk_bytes = match next {
                    Ok(chunk_bytes) => chunk_bytes,
                    Err(error) => {
                        yield error_event(InferenceError::RequestFailed {
                            message: error.to_string(),
                        });
                        return;
                    }
                };

                for data in sse.push(&chunk_bytes) {
                    if data == "[DONE]" {
                        state.message.finish_reason = Some(FinishReason::Stop);
                        yield AssistantMessageEvent::Done { message: state.message.clone() };
                        return;
                    }

                    if inference_request.options.include_raw
                        && let Ok(raw) = serde_json::from_str::<Value>(&data)
                    {
                        yield AssistantMessageEvent::RawProviderEvent {
                            provider: model.provider.0.clone(),
                            payload: raw,
                        };
                    }

                    let chunk = match serde_json::from_str::<OpenAiChatChunk>(&data) {
                        Ok(chunk) => chunk,
                        Err(error) => {
                            yield error_event(InferenceError::Json {
                                message: error.to_string(),
                            });
                            return;
                        }
                    };

                    for event in state.apply_chunk(chunk) {
                        yield event;
                    }
                }
            }

            state.message.finish_reason = Some(FinishReason::Unknown);
            yield AssistantMessageEvent::Done {
                message: state.message,
            };
        })
    }
}

#[derive(Debug, Serialize)]
struct OpenAiChatRequest {
    model: String,
    messages: Vec<OpenAiMessage>,
    stream: bool,

    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<OpenAiTool>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,

    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f32>,

    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,

    stream_options: OpenAiStreamOptions,
}

#[derive(Debug, Serialize)]
struct OpenAiStreamOptions {
    include_usage: bool,
}

impl OpenAiChatRequest {
    #[expect(
        clippy::single_call_fn,
        reason = "keeps request validation and mapping colocated"
    )]
    fn try_from_resolved(
        model: &ModelSpec,
        request: &InferenceRequest,
    ) -> Result<Self, InferenceError> {
        if model.api_family != ApiFamily::OpenAiChatCompletions {
            return Err(InferenceError::UnsupportedFeature {
                api_family: model.api_family.clone(),
                feature: "openai-compatible chat completions adapter".to_owned(),
            });
        }

        Ok(Self {
            model: model.model_id.0.clone(),
            messages: map_messages(&request.context),
            stream: true,
            tools: if request.tools.is_empty() {
                None
            } else {
                Some(request.tools.iter().map(OpenAiTool::from).collect())
            },
            temperature: request.options.temperature,
            top_p: request.options.top_p,
            max_tokens: request.options.max_output_tokens,
            stream_options: OpenAiStreamOptions {
                include_usage: true,
            },
        })
    }
}

#[derive(Debug, Serialize)]
#[serde(tag = "role", rename_all = "snake_case")]
enum OpenAiMessage {
    System {
        content: String,
    },
    User {
        content: String,
    },
    Assistant {
        content: String,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<OpenAiToolCall>,
    },
    Tool {
        tool_call_id: String,
        content: String,
    },
}

#[derive(Debug, Clone, Serialize)]
struct OpenAiTool {
    #[serde(rename = "type")]
    kind: &'static str,
    function: OpenAiFunction,
}

#[derive(Debug, Clone, Serialize)]
struct OpenAiFunction {
    name: String,
    description: String,
    parameters: Value,
}

impl From<&ToolDefinition> for OpenAiTool {
    fn from(tool: &ToolDefinition) -> Self {
        Self {
            kind: "function",
            function: OpenAiFunction {
                name: tool.name.0.clone(),
                description: tool.description.clone(),
                parameters: tool.parameters.clone(),
            },
        }
    }
}

#[derive(Debug, Serialize)]
struct OpenAiToolCall {
    id: String,
    #[serde(rename = "type")]
    kind: &'static str,
    function: OpenAiToolCallFunction,
}

#[derive(Debug, Serialize)]
struct OpenAiToolCallFunction {
    name: String,
    arguments: String,
}

#[expect(
    clippy::single_call_fn,
    reason = "names the provider-specific context mapping boundary"
)]
fn map_messages(context: &InferenceContext) -> Vec<OpenAiMessage> {
    let mut out = Vec::new();

    for message in context.messages.clone() {
        match message {
            Message::System(system) => out.push(OpenAiMessage::System {
                content: system.content,
            }),
            Message::User(user) => out.push(OpenAiMessage::User {
                content: blocks_to_text(&user.content),
            }),
            Message::Assistant(assistant) => out.push(OpenAiMessage::Assistant {
                content: blocks_to_text(&assistant.content),
                tool_calls: Vec::new(),
            }),
            Message::Tool(tool) => out.push(OpenAiMessage::Tool {
                tool_call_id: tool.tool_call_id,
                content: blocks_to_text(&tool.content),
            }),
        }
    }

    out
}

fn blocks_to_text(blocks: &[ContentBlock]) -> String {
    let mut out = String::new();

    for block in blocks.iter().cloned() {
        match block {
            ContentBlock::Text { text } => out.push_str(&text),
            ContentBlock::Thinking { text } => {
                out.push_str("<thinking>");
                out.push_str(&text);
                out.push_str("</thinking>");
            }
            ContentBlock::Image { .. } => {
                out.push_str("[image omitted]");
            }
        }
    }

    out
}

#[derive(Default)]
struct AssistantState {
    message: AssistantMessage,
}

impl AssistantState {
    fn apply_chunk(&mut self, chunk: OpenAiChatChunk) -> Vec<AssistantMessageEvent> {
        let mut events = Vec::new();

        for choice in chunk.choices {
            if let Some(delta) = choice.delta.content {
                self.message.content.push(ContentBlock::Text {
                    text: delta.clone(),
                });

                events.push(AssistantMessageEvent::TextDelta {
                    delta,
                    message: self.message.clone(),
                });
            }

            if let Some(tool_calls) = choice.delta.tool_calls {
                for tool_call in tool_calls {
                    let delta = ToolCallDelta {
                        id: tool_call.id,
                        index: tool_call.index,
                        name: tool_call
                            .function
                            .as_ref()
                            .and_then(|function_delta| function_delta.name.clone()),
                        arguments_delta: tool_call
                            .function
                            .as_ref()
                            .and_then(|function_delta| function_delta.arguments.clone()),
                    };

                    self.message.tool_calls.push(delta.clone());

                    events.push(AssistantMessageEvent::ToolCallDelta {
                        delta,
                        message: self.message.clone(),
                    });
                }
            }

            if let Some(reason) = choice.finish_reason {
                self.message.finish_reason = Some(map_finish_reason(&reason));
            }
        }
        events
    }
}

#[expect(
    clippy::single_call_fn,
    reason = "documents provider finish reason normalization"
)]
fn map_finish_reason(reason: &str) -> FinishReason {
    match reason {
        "stop" => FinishReason::Stop,
        "length" => FinishReason::Length,
        "tool_calls" => FinishReason::ToolCalls,
        "content_filter" => FinishReason::ContentFilter,
        _ => FinishReason::Unknown,
    }
}

#[derive(Debug, Deserialize)]
struct OpenAiChatChunk {
    choices: Vec<OpenAiChoice>,
}

#[derive(Debug, Deserialize)]
struct OpenAiChoice {
    delta: OpenAiDelta,
    finish_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenAiDelta {
    content: Option<String>,
    tool_calls: Option<Vec<OpenAiToolCallDelta>>,
}

#[derive(Debug, Deserialize)]
struct OpenAiToolCallDelta {
    index: usize,
    id: Option<String>,
    function: Option<OpenAiToolFunctionDelta>,
}

#[derive(Debug, Deserialize)]
struct OpenAiToolFunctionDelta {
    name: Option<String>,
    arguments: Option<String>,
}

#[derive(Default)]
struct SseDecoder {
    buffer: String,
}

impl SseDecoder {
    fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.buffer.push_str(&String::from_utf8_lossy(bytes));

        let mut events = Vec::new();

        while let Some((raw_event, remaining_buffer)) = self.buffer.split_once("\n\n") {
            let event_text = raw_event.to_owned();
            self.buffer = remaining_buffer.to_owned();

            let mut data_lines = Vec::new();

            for line in event_text.lines() {
                if let Some(data) = line.strip_prefix("data:") {
                    data_lines.push(data.trim().to_owned());
                }
            }

            if !data_lines.is_empty() {
                events.push(data_lines.join("\n"));
            }
        }

        events
    }
}

fn error_event(error: InferenceError) -> AssistantMessageEvent {
    AssistantMessageEvent::Error {
        error,
        message: AssistantMessage {
            content: Vec::new(),
            tool_calls: Vec::new(),
            finish_reason: Some(FinishReason::Error),
            usage: None,
        },
    }
}
