use async_stream::stream;
use futures::StreamExt;
use reqwest::Client;
use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    context::{AssistantMessage, ContentBlock, InferenceContext, Message},
    error::InferenceError,
    event::{
        AssistantMessageEvent, FinishReason, InferenceStream, ToolCallAccumulator, ToolCallDelta,
    },
    model::{ApiFamily, ModelSpec},
    provider::{InferenceProvider, InferenceRequest, ResolvedInferenceRequest},
    providers::{error_event, sse::SseDecoder},
    tool::ToolDefinition,
    usage::TokenUsage,
};

/// Provider adapter for the official OpenAI Responses API.
///
/// This adapter owns only protocol translation:
/// - cupel request -> OpenAI JSON payload
/// - OpenAI SSE stream -> cupel assistan events
///
/// It does not load API keys from the environment. The CLI/runtime injects the
/// key into `InferenceRequestOptions`.
#[derive(Clone)]
pub struct OpenAiResponseProvider {
    http: Client,
}

impl OpenAiResponseProvider {
    #[must_use]
    pub fn new() -> Self {
        Self {
            http: Client::new(),
        }
    }
}

impl Default for OpenAiResponseProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl InferenceProvider for OpenAiResponseProvider {
    fn stream(&self, resolved: ResolvedInferenceRequest) -> InferenceStream {
        let http = self.http.clone();

        Box::pin(stream! {
            let model = resolved.model;
            let request = resolved.request;

            let Some(base_url) = model.base_url.clone() else {
                yield error_event(InferenceError::InvalidBaseUrl {
                    base_url: "<missing>".to_owned(),
                });
                return;
            };

            let Some(api_key) = request.options.api_key.clone() else {
                yield error_event(InferenceError::MissingApiKey {
                    provider: model.provider.0.clone(),
                });
                return;
            };

            let payload = match OpenAiResponsesRequest::try_from_request(&model, &request) {
                Ok(payload) => payload,
                Err(error) => {
                    yield error_event(error);
                    return;
                }
            };

            let url = format!("{}/responses", base_url.trim_end_matches('/'));

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
                        message: error.to_string()
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

            let mut state = OpenAiResponsesState::default();

            // Emit a provider-neutral start event immediatly. Some providers
            // also send their own "created" event, but cupel consumers should
            // always see a stable Start event first.
            yield AssistantMessageEvent::Start {
                message: state.message.clone(),
            };

            let mut decoder = SseDecoder::default();
            let mut bytes = response.bytes_stream();

            while let Some(next) = bytes.next().await {
                let bytes = match next {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        yield error_event(InferenceError::RequestFailed {
                            message: error.to_string(),
                        });
                        return;
                    }
                };

                for sse in decoder.push(&bytes) {
                    if request.options.include_raw && let Ok(raw) = serde_json::from_str::<Value>(&sse.data) {
                        yield AssistantMessageEvent::RawProviderEvent { provider: model.provider.0.clone(), payload: raw, };
                    }

                    let provider_event = match serde_json::from_str::<OpenAiResponsesEvent>(&sse.data) {
                        Ok(event) => event,
                        Err(error) => {
                            yield error_event(InferenceError::Json { message: error.to_string(), });
                            return;
                        }
                    };

                    for event in state.apply_event(provider_event) {
                        yield event;
                    }

                    if state.done {
                        return;
                    }
                }
            }

            // If the HTTP stream ends without a formal completion event, return
            // what we have and mark the finish reason as unknown.
            state.message.finish_reason = Some(FinishReason::Unknown);
            yield AssistantMessageEvent::Done { message: state.message, };
        })
    }
}

/// JSON body sent to `POST /v1/responses`.
///
/// Keep this struct provider-specific. Do not leak OpenAi wire names into the
/// provider-neutral request types.
#[derive(Debug, Serialize)]
struct OpenAiResponsesRequest {
    model: String,
    input: Vec<OpenAiInputItem>,
    stream: bool,
    store: bool,

    #[serde(skip_serializing_if = "Option::is_none")]
    instructions: Option<String>,

    #[serde(skip_serializing_if = "Option::is_none")]
    tools: Option<Vec<OpenAiResponsesTool>>,

    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,

    #[serde(skip_serializing_if = "Option::is_none")]
    top_p: Option<f32>,

    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokes: Option<u32>,

    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<OpenAiReasoning>,
}

#[derive(Debug, Serialize)]
struct OpenAiReasoning {
    /// OpenAI Responses supports effort values for reasoning models.
    ///
    /// cupel's provider-neutral enum maps into this provider-specific string.
    effort: String,

    /// Ask for a summary rather than hidden chain-of-thought.
    ///
    /// This keeps the provider adapter aligned with visible reasoning only.
    summary: String,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum OpenAiInputItem {
    Message {
        role: OpenAiMessageRole,
        content: Vec<OpenAiContentPart>,
    },
    FunctionalCall {
        call_id: String,
        name: String,
        arguments: String,
    },
    FunctionCallOutput {
        call_id: String,
        output: String,
    },
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum OpenAiMessageRole {
    User,
    Assistant,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum OpenAiContentPart {
    InputText { text: String },
    Output { text: String },
}

#[derive(Debug, Serialize)]
struct OpenAiResponsesTool {
    #[serde(rename = "type")]
    kind: &'static str,
    name: String,
    description: String,
    parameters: Value,
}

impl OpenAiResponsesRequest {
    fn try_from_request(
        model: &ModelSpec,
        request: &InferenceRequest,
    ) -> Result<Self, InferenceError> {
        let (instructions, input) = map_context_to_response_input(&request.context)?;

        Ok(Self {
            model: model.model_id.0.clone(),
            input,
            stream: true,

            // `store: false` keeps cupel in control of conversation state. The
            // runtime will persist context later; the provider should not depend
            // on provider-side state for correctness.
            store: false,

            instructions,
            tools: map_tools(&request.tools),
            temperature: request.options.temperature,
            top_p: request.options.top_p,
            max_output_tokes: request.options.max_output_tokens,
            reasoning: map_reasoning(&request.options),
        })
    }
}

fn map_context_to_response_input(
    context: &InferenceContext,
) -> Result<(Option<String>, Vec<OpenAiInputItem>), InferenceError> {
    let mut system_parts = Vec::new();
    let mut input = Vec::new();

    for message in &context.messages {
        match message {
            Message::System(system) => {
                // Responses has a top-level `instructions` field. Joining
                // multiple system messages keeps the provider-neutral context
                // flexible while producing a single OpenAI instruction string.
                system_parts.push(system.content.clone());
            }
            Message::User(user) => {
                input.push(OpenAiInputItem::Message {
                    role: OpenAiMessageRole::User,
                    content: map_input_content_blocks(&user.content)?,
                });
            }
            Message::Assistant(assistant) => {
                let text = blocks_to_text(&assistant.content)?;
                if !text.is_empty() {
                    input.push(OpenAiInputItem::Message {
                        role: OpenAiMessageRole::Assistant,
                        content: vec![OpenAiContentPart::Output { text }],
                    });
                }

                // Completed tool calls from previous assistant turns must be
                // replayed so the provider sees the full conversation.
                for call in &assistant.tool_calls {
                    input.push(OpenAiInputItem::FunctionalCall {
                        call_id: call
                            .id
                            .clone()
                            .unwrap_or_else(|| format!("call_{}", call.index)),
                        name: call.name.clone(),
                        arguments: call.raw_arguments.clone(),
                    });
                }
            }
            Message::Tool(tool) => {
                input.push(OpenAiInputItem::FunctionCallOutput {
                    call_id: tool.tool_call_id.clone(),
                    output: blocks_to_text(&tool.content)?,
                });
            }
        }
    }

    let instructions = if system_parts.is_empty() {
        None
    } else {
        Some(system_parts.join("\n\n"))
    };

    Ok((instructions, input))
}

fn map_input_content_blocks(
    blocks: &[ContentBlock],
) -> Result<Vec<OpenAiContentPart>, InferenceError> {
    let mut parts = Vec::new();

    for block in blocks {
        match block {
            ContentBlock::Text { text } => {
                parts.push(OpenAiContentPart::InputText { text: text.clone() });
            }
            ContentBlock::Thinking { text } => {
                // Visible thinking is preserved as text. Hidden provider
                // reasoning should never be stored in this block.
                parts.push(OpenAiContentPart::InputText {
                    text: format!("<thinking>{text}</thinking>"),
                });
            }
            ContentBlock::Image { .. } => {
                // TODO: Add real image mapping
                return Err(InferenceError::UnsupportedFeature {
                    api_family: ApiFamily::OpenAiResponses,
                    feature: "image input in OpenAI Responses adapter".to_owned(),
                });
            }
        }
    }

    Ok(parts)
}

fn map_tools(tools: &[ToolDefinition]) -> Option<Vec<OpenAiResponsesTool>> {
    if tools.is_empty() {
        return None;
    }

    Some(
        tools
            .iter()
            .map(|tool| OpenAiResponsesTool {
                kind: "function",
                name: tool.name.0.clone(),
                description: tool.description.clone(),
                parameters: tool.parameters.clone(),
            })
            .collect(),
    )
}

/// Minimal subset of OpenAI Responses streaming events cupel needs for v1.
///
/// Unknown fields are intentionally ignored. Unknown event types should not
/// crash the stream unless they are reuqired for correctness.
#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
enum OpenAiResponsesEvent {
    #[serde(rename = "response.output_text.delta")]
    OutputTextDelta { delta: String },

    #[serde(rename = "response.reasoning_summary_text.delta")]
    ReasoningSummaryTextDelta { delta: String },

    #[serde(rename = "response.reasoning_text.delta")]
    ReasoningTextDelta { delta: String },

    #[serde(rename = "response.output_item.added")]
    OutputItemAdded {
        output_index: usize,
        item: OpenAiOutputItem,
    },

    #[serde(rename = "response.function_call_arguments.delta")]
    FunctionCallArgumentsDelta { output_index: usize, delta: String },

    #[serde(rename = "response.function_call_arguments.done")]
    FunctionCallArgumentsDone {
        output_index: usize,
        arguments: String,
    },

    #[serde(rename = "response.completed")]
    Completed { response: OpenAiCompletedResponse },

    #[serde(rename = "response.failed")]
    Failed { response: OpenAiCompletedResponse },

    #[serde(rename = "error")]
    Error { response: Option<String> },

    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
struct OpenAiOutputItem {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    call_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenAiCompletedResponse {
    #[serde(default)]
    usage: Option<OpenAiUsage>,
    #[serde(default)]
    status: Option<String>,
}

#[derive(Default)]
struct OpenAiResponsesState {
    message: AssistantMessage,
    tool_calls: ToolCallAccumulator,
    done: bool,
}

impl OpenAiResponsesState {
    fn apply_event(&mut self, event: OpenAiResponsesEvent) -> Vec<AssistantMessageEvent> {
        let mut out = Vec::new();

        match event {
            OpenAiResponsesEvent::OutputTextDelta { delta } => {
                self.message.content.push(ContentBlock::Text {
                    text: delta.clone(),
                });
                out.push(AssistantMessageEvent::TextDelta {
                    delta,
                    message: self.message.clone(),
                });
            }
            OpenAiResponsesEvent::ReasoningSummaryTextDelta { delta }
            | OpenAiResponsesEvent::ReasoningTextDelta { delta } => {
                self.message.content.push(ContentBlock::Thinking {
                    text: delta.clone(),
                });
                out.push(AssistantMessageEvent::ThinkingDelta {
                    delta,
                    message: self.message.clone(),
                });
            }
            OpenAiResponsesEvent::OutputItemAdded { output_index, item } => {
                let delta = ToolCallDelta {
                    id: item.call_id.or(item.id),
                    index: output_index,
                    name: item.name,
                    arguments_delta: None,
                };
                self.tool_calls.push_delta(delta.clone());
                out.push(AssistantMessageEvent::ToolCallDelta {
                    delta,
                    message: self.message.clone(),
                });
            }
            OpenAiResponsesEvent::FunctionCallArgumentsDelta {
                output_index,
                delta,
            } => {
                let delta = ToolCallDelta {
                    id: None,
                    index: output_index,
                    name: None,
                    arguments_delta: Some(delta),
                };
                self.tool_calls.push_delta(delta.clone());
                out.push(AssistantMessageEvent::ToolCallDelta {
                    delta,
                    message: self.message.clone(),
                });
            }
            OpenAiResponsesEvent::FunctionCallArgumentsDone {
                output_index,
                arguments,
            } => {
                // Some streams send a final full argument string. Treat as
                // authoritative for that call. The accumulator API can expose a
                // replace/finalize method if this becomes necessary.
                let delta = ToolCallDelta {
                    id: None,
                    index: output_index,
                    name: None,
                    arguments_delta: Some(arguments),
                };
                self.tool_calls.push_delta(delta.clone());
                out.push(AssistantMessageEvent::ToolCallDelta {
                    delta,
                    message: self.message.clone(),
                });
            }
            OpenAiResponsesEvent::Completed { response } => {
                self.message.finish_reason = Some(FinishReason::Stop);
                self.message.usage = response.usage.map(map_openai_usage);
                self.done = true;
                out.push(AssistantMessageEvent::Done {
                    message: self.message.clone(),
                });
            }
            OpenAiResponsesEvent::Failed { response } => {
                self.message.finish_reason = Some(FinishReason::Error);
                self.message.usage = response.usage.map(map_openai_usage);
                self.done = true;
                out.push(AssistantMessageEvent::Error {
                    error: InferenceError::ProviderProtocol {
                        message: "OpenAI Responses stream failed".to_owned(),
                    },
                    message: self.message.clone(),
                });
            }
            OpenAiResponsesEvent::Error { response } => {
                self.message.finish_reason = Some(FinishReason::Error);
                self.done = true;
                out.push(AssistantMessageEvent::Error {
                    error: InferenceError::ProviderProtocol {
                        message: response
                            .unwrap_or_else(|| "OpenAI Responses stream error".to_owned()),
                    },
                    message: self.message.clone(),
                });
            }
            OpenAiResponsesEvent::Unknown => {}
        }

        out
    }
}

#[derive(Debug, Deserialize)]
struct OpenAiUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    input_tokens_details: Option<OpenAiInputTokenDetails>,
    #[serde(default)]
    output_tokens_details: Option<OpenAiOutputTokenDetails>,
}

#[derive(Debug, Deserialize)]
struct OpenAiInputTokenDetails {
    #[serde(default)]
    cached_tokens: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct OpenAiOutputTokenDetails {
    #[serde(default)]
    reasoning_tokens: Option<u64>,
}

fn map_openai_usage(usage: OpenAiUsage) -> TokenUsage {
    TokenUsage {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cached_input_tokens: usage
            .input_tokens_details
            .and_then(|details| details.cached_tokens),
        cached_output_tokens: None,
        // If `TokenUsage` does not have this field yet, add it in Phase A or
        // skip it until the usage type is expanded.
        reasoning_tokens: usage
            .output_tokens_details
            .and_then(|details| details.reasoning_tokens),
    }
}
