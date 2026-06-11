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
    provider::{
        InferenceProvider, InferenceRequest, InferenceRequestOptions, ReasoningEffort,
        ResolvedInferenceRequest,
    },
    providers::{error_event, sse::SseDecoder},
    tool::ToolDefinition,
    usage::TokenUsage,
};

/// Provider adapter for the official `OpenAI` Responses API.
///
/// This adapter owns only protocol translation:
/// - cupel request -> `OpenAI` JSON payload
/// - `OpenAI` SSE stream -> cupel assistan events
///
/// Does not load API keys from the environment. The CLI/runtime injects the
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

#[expect(
    clippy::renamed_function_params,
    reason = "Confusion with seperatly defined `request`"
)]
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
/// Keep this struct provider-specific. Do not leak `OpenAi` wire names into the
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
    max_output_tokens: Option<u32>,

    #[serde(skip_serializing_if = "Option::is_none")]
    reasoning: Option<OpenAiReasoning>,
}

#[derive(Debug, Serialize)]
struct OpenAiReasoning {
    /// `OpenAI` Responses supports effort values for reasoning models.
    ///
    /// cupel's provider-neutral enum maps into this provider-specific string.
    effort: String,

    /// Ask for a summary rather than hidden chain-of-thought.
    ///
    /// This keeps the provider adapter aligned with visible reasoning only.
    summary: String,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum OpenAiInputItem {
    Message {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        content: Vec<OpenAiOutputContentPart>,
        #[serde(default)]
        status: Option<String>,
    },

    Reasoning {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        summary: Vec<OpenAiReasoningSummaryPart>,
        #[serde(default)]
        content: Vec<OpenAiReasoningTextPart>,
    },
    
    FunctionCall {
        #[serde(default)]
        id: Option<String>,
        call_id: String,
        name: String,
        #[serde(default)]
        arguments: String,
    },

    /// Unknown output items are ignored by the neutral state machine, while the
    /// original raw event can still be emitted when `include_raw` is enabled.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "snake_case")]
enum OpenAiMessageRole {
    User,
    Assistant,
}

#[derive(Debug, Deserialize)]
enum OpenAiOutputContentPart {
    OutputText {
        #[serde(default)]
        text: String,
    },
    Refusal {
        #[serde(default)]
        refusal: String,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
struct OpenAiReasoningSummaryPart {
    #[serde(default)]
    text: String,
}

#[derive(Debug, Deserialize)]
struct OpenAiReasoningTextPart {
    #[serde(default)]
    text: String,
}

#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum OpenAiContentPart {
    InputText { text: String },
    OutputText { text: String },
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
            max_output_tokens: request.options.max_output_tokens,
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
                        content: vec![OpenAiContentPart::OutputText { text }],
                    });
                }

                // Completed tool calls from previous assistant turns must be
                // replayed so the provider sees the full conversation.
                for call in &assistant.tool_calls {
                    input.push(OpenAiInputItem::FunctionCall {
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

fn blocks_to_text(blocks: &[ContentBlock]) -> Result<String, InferenceError> {
    let mut out = String::new();

    for block in blocks {
        match block {
            ContentBlock::Text { text } => {
                // Text blocks may be split across provider deltas. Joining
                // without extra separators preserves the original stream.
                out.push_str(text);
            }
            ContentBlock::Thinking { text } => {
                // This is visible reasoning content that cupel already chose
                // to store. Preserve the boundary so replayed assistant/tool
                // messages do not make thinking look like normal answer text.
                out.push_str("<thinking>");
                out.push_str(text);
                out.push_str("</thinking>");
            }
            ContentBlock::Image { .. } => {
                // Placeholder because adapter does not have image replay wired yet.
                return Err(InferenceError::UnsupportedFeature {
                    api_family: ApiFamily::OpenAiResponses,
                    feature: "image content  replay in OpenAI Responses adapter".to_owned(),
                });
            }
        }
    }

    Ok(out)
}

fn map_reasoning(options: &InferenceRequestOptions) -> Option<OpenAiReasoning> {
    let effort = match *options.reasoning_effort.as_ref()? {
        ReasoningEffort::Low => "low",
        ReasoningEffort::Medium => "medium",
        ReasoningEffort::High => "high",
    };

    Some(OpenAiReasoning {
        // OpenAI expects a lowercase string in the Responeses `reasoning`
        // object.
        effort: effort.to_owned(),

        // cupel stores only visible reasoning summaries in
        // ContentBlock::Thinking. `summary: "auto"` opts in to the most useful
        // summary the model supports without exposing hidden chain-of-thought.
        summary: "auto".to_owned(),
    })
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

/// Minimal subset of `OpenAI` Responses streaming events cupel needs for v1.
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
#[serde(tag = "type", rename_all = "snake_case")]
enum OpenAiOutputItem {
    Message {},
    Reasoning {},
    FunctionCall {
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        call_id: Option<String>,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
struct OpenAiCompletedResponse {
    #[serde(default)]
    usage: Option<OpenAiUsage>,
    #[serde(default)]
    status: Option<OpenAiResponseStatus>,
    #[serde(default)]
    incomplete_details: Option<OpenAiIncompleteDetails>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum OpenAiResponseStatus {
    Completed,
    Incomplete,
    Failed,
    Cancelled,
    Queued,
    InProgress,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Deserialize)]
struct OpenAiIncompleteDetails {
    #[serde(default)]
    reason: Option<String>,
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
                match item {
                    OpenAiOutputItem::Message { .. } => {
                        self.start_item(output_index, ActiveOutputKind::Message);
                    }
                    OpenAiOutputItem::Reasoning { .. } => {
                        self.start_item(output_index, ActiveOutputKind::Reasoning);
                    }
                    OpenAiOutputItem::FunctionCall { id, name, call_id , arguments } => {
                        self.start_function_call(output_index, id, call_id, name, arguments);
                    }
                    OpenAiOutputItem::Unknown => {}
                }
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
                // The done event carries the final authoritative JSON string,
                // not another incremental delta. Replace the accumulator buffer
                // so finalization does not duplicate already-streamed text. If
                // the final string merely extends the current buffer, emit only
                // the suffix as a normal provider-neutral delta.
                if let Some(arguments_delta) = self
                    .tool_calls
                    .replace_arguments(output_index, arguments)
                    .filter(|suffix| !suffix.is_empty())
                {
                    let delta = ToolCallDelta {
                        id: None,
                        index: output_index,
                        name: None,
                        arguments_delta: Some(arguments_delta),
                    };
                    out.push(AssistantMessageEvent::ToolCallDelta {
                        delta,
                        message: self.message.clone(),
                    });
                }
            }
            OpenAiResponsesEvent::Completed { response } => {
                self.message.usage = response.usage.map(map_openai_usage);

                if let Err(error) = self.finalize_tool_calls() {
                    self.message.finish_reason = Some(FinishReason::Error);
                    self.done = true;
                    out.push(AssistantMessageEvent::Error {
                        error,
                        message: self.message.clone(),
                    });
                } else {
                    self.message.finish_reason = Some(map_response_status(
                        response.status,
                        response.incomplete_details.as_ref(),
                        !self.message.tool_calls.is_empty(),
                    ));
                    self.done = true;
                    out.push(AssistantMessageEvent::Done {
                        message: self.message.clone(),
                    });
                }
            }
            OpenAiResponsesEvent::Failed { response } => {
                self.message.finish_reason = Some(FinishReason::Error);
                self.message.usage = response.usage.map(map_openai_usage);
                self.done = true;
                out.push(AssistantMessageEvent::Error {
                    error: InferenceError::ProviderProtocol {
                        message: response
                            .status
                            .map(|status| {
                                format!("openai responses stream failed with status {status:?}")
                            })
                            .unwrap_or_else(|| "openai responses stream failed".to_owned()),
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

    fn finalize_tool_calls(&mut self) -> Result<(), InferenceError> {
        let mut tool_calls = Vec::new();

        for call in self.tool_calls.finish_all() {
            let call = call.map_err(|error| InferenceError::ProviderProtocol {
                message: format!("malformed openai responses tool-call arguments: {error}"),
            })?;
            tool_calls.push(call);
        }

        self.message.tool_calls = tool_calls;
        Ok(())
    }
}

fn map_response_status(
    status: Option<OpenAiResponseStatus>,
    incomplete_details: Option<&OpenAiIncompleteDetails>,
    has_tool_calls: bool,
) -> FinishReason {
    match status {
        Some(OpenAiResponseStatus::Incomplete) => {
            if incomplete_details
                .and_then(|details| details.reason.as_deref())
                .is_some_and(|reason| reason == "content_filter")
            {
                FinishReason::ContentFilter
            } else {
                FinishReason::Length
            }
        }
        Some(OpenAiResponseStatus::Failed | OpenAiResponseStatus::Cancelled) => FinishReason::Error,
        Some(
            OpenAiResponseStatus::Completed
            | OpenAiResponseStatus::Queued
            | OpenAiResponseStatus::InProgress
            | OpenAiResponseStatus::Unknown,
        )
        | None => {
            if has_tool_calls {
                FinishReason::ToolCalls
            } else {
                FinishReason::Stop
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_to_text_preserves_text_and_visible_thinking() {
        let blocks = vec![
            ContentBlock::Text {
                text: "answer ".to_owned(),
            },
            ContentBlock::Thinking {
                text: "checked the tool result".to_owned(),
            },
            ContentBlock::Text {
                text: " done".to_owned(),
            },
        ];

        let text = blocks_to_text(&blocks).expect("text blocks should map");

        assert_eq!(
            text,
            "answer <thinking>checked the tool result</thinking> done"
        );
    }

    #[test]
    fn blocks_to_text_rejects_images() {
        let blocks = vec![ContentBlock::Image {
            media: "image/png".to_owned(),
            data: Vec::new(),
        }];

        assert!(matches!(
            blocks_to_text(&blocks),
            Err(InferenceError::UnsupportedFeature {
                api_family: ApiFamily::OpenAiResponses,
                ..
            })
        ));
    }

    #[test]
    fn map_reasoning_omits_reasoning_when_no_effort_is_set() {
        let options = InferenceRequestOptions::default();

        assert!(map_reasoning(&options).is_none());
    }

    #[test]
    fn map_reasoning_maps_provider_neutral_effort_to_openai_wire_value() {
        let mut options = InferenceRequestOptions::default();
        options.reasoning_effort = Some(ReasoningEffort::High);

        let reasoning = map_reasoning(&options).expect("effort should map");

        assert_eq!(reasoning.effort, "high");
        assert_eq!(reasoning.summary, "auto");
    }

    #[test]
    fn output_item_added_ignores_message_and_reasoning_items() {
        for item in [OpenAiOutputItem::Message {}, OpenAiOutputItem::Reasoning {}] {
            let mut state = OpenAiResponsesState::default();

            let events = state.apply_event(OpenAiResponsesEvent::OutputItemAdded {
                output_index: 0,
                item,
            });

            assert!(events.is_empty());
            assert!(state.tool_calls.finish_all().is_empty());
        }
    }

    #[test]
    fn output_item_added_starts_function_call_items() {
        let mut state = OpenAiResponsesState::default();

        let events = state.apply_event(OpenAiResponsesEvent::OutputItemAdded {
            output_index: 2,
            item: OpenAiOutputItem::FunctionCall {
                id: Some("item_1".to_owned()),
                call_id: Some("call_1".to_owned()),
                name: Some("read".to_owned()),
            },
        });

        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            AssistantMessageEvent::ToolCallDelta {
                delta: ToolCallDelta {
                    id: Some(id),
                    index: 2,
                    name: Some(name),
                    arguments_delta: None,
                },
                ..
            } if id == "call_1" && name == "read"
        ));
    }

    #[test]
    fn output_item_added_ignores_unknown_items() {
        let event = serde_json::from_value::<OpenAiResponsesEvent>(serde_json::json!({
            "type": "response.output_item.added",
            "output_index": 0,
            "item": {
                "type": "web_search_call",
                "id": "ws_1"
            }
        }))
        .expect("unknown output item should deserialize");

        let mut state = OpenAiResponsesState::default();
        let events = state.apply_event(event);

        assert!(events.is_empty());
    }

    #[test]
    fn completed_finalizes_accumulated_tool_calls() {
        let mut state = OpenAiResponsesState::default();

        state.apply_event(OpenAiResponsesEvent::OutputItemAdded {
            output_index: 1,
            item: OpenAiOutputItem::FunctionCall {
                id: Some("item_1".to_owned()),
                call_id: Some("call_1".to_owned()),
                name: Some("read".to_owned()),
            },
        });
        state.apply_event(OpenAiResponsesEvent::FunctionCallArgumentsDelta {
            output_index: 1,
            delta: r#"{"path":"README.md"}"#.to_owned(),
        });

        let events = state.apply_event(OpenAiResponsesEvent::Completed {
            response: OpenAiCompletedResponse {
                usage: None,
                status: Some(OpenAiResponseStatus::Completed),
                incomplete_details: None,
            },
        });

        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            AssistantMessageEvent::Done { message }
                if message.tool_calls.len() == 1
                    && message.tool_calls[0].id.as_deref() == Some("call_1")
                    && message.tool_calls[0].index == 1
                    && message.tool_calls[0].name == "read"
                    && message.tool_calls[0].arguments["path"] == "README.md"
                    && message.tool_calls[0].raw_arguments == r#"{"path":"README.md"}"#
                    && message.finish_reason == Some(FinishReason::ToolCalls)
        ));
        assert_eq!(state.message.tool_calls.len(), 1);
    }

    #[test]
    fn function_call_arguments_done_replaces_accumulated_arguments() {
        let mut state = OpenAiResponsesState::default();

        state.apply_event(OpenAiResponsesEvent::OutputItemAdded {
            output_index: 0,
            item: OpenAiOutputItem::FunctionCall {
                id: None,
                call_id: Some("call_1".to_owned()),
                name: Some("read".to_owned()),
            },
        });
        state.apply_event(OpenAiResponsesEvent::FunctionCallArgumentsDelta {
            output_index: 0,
            delta: r#"{"path"#.to_owned(),
        });

        let events = state.apply_event(OpenAiResponsesEvent::FunctionCallArgumentsDone {
            output_index: 0,
            arguments: r#"{"path":"README.md"}"#.to_owned(),
        });

        assert_eq!(events.len(), 1);
        assert!(matches!(
            &events[0],
            AssistantMessageEvent::ToolCallDelta {
                delta: ToolCallDelta {
                    index: 0,
                    arguments_delta: Some(arguments_delta),
                    ..
                },
                ..
            } if arguments_delta == "\":\"README.md\"}"
        ));

        let events = state.apply_event(OpenAiResponsesEvent::Completed {
            response: OpenAiCompletedResponse {
                usage: None,
                status: Some(OpenAiResponseStatus::Completed),
                incomplete_details: None,
            },
        });

        assert!(matches!(
            &events[0],
            AssistantMessageEvent::Done { message }
                if message.tool_calls[0].raw_arguments == r#"{"path":"README.md"}"#
                    && message.tool_calls[0].arguments["path"] == "README.md"
        ));
    }

    #[test]
    fn completed_reports_error_for_malformed_tool_call_arguments() {
        let mut state = OpenAiResponsesState::default();

        state.apply_event(OpenAiResponsesEvent::OutputItemAdded {
            output_index: 0,
            item: OpenAiOutputItem::FunctionCall {
                id: None,
                call_id: Some("call_bad".to_owned()),
                name: Some("read".to_owned()),
            },
        });
        state.apply_event(OpenAiResponsesEvent::FunctionCallArgumentsDelta {
            output_index: 0,
            delta: "not json".to_owned(),
        });

        let events = state.apply_event(OpenAiResponsesEvent::Completed {
            response: OpenAiCompletedResponse {
                usage: None,
                status: Some(OpenAiResponseStatus::Completed),
                incomplete_details: None,
            },
        });

        assert_eq!(events.len(), 1);
        assert!(state.done);
        assert_eq!(state.message.finish_reason, Some(FinishReason::Error));
        assert!(matches!(
            &events[0],
            AssistantMessageEvent::Error {
                error: InferenceError::ProviderProtocol { message },
                ..
            } if message.contains("malformed openai responses tool-call arguments")
        ));
    }

    #[test]
    fn map_response_status_maps_incomplete_to_length() {
        assert_eq!(
            map_response_status(Some(OpenAiResponseStatus::Incomplete), None, false),
            FinishReason::Length
        );
    }

    #[test]
    fn map_response_status_maps_incomplete_content_filter_reason() {
        let details = OpenAiIncompleteDetails {
            reason: Some("content_filter".to_owned()),
        };

        assert_eq!(
            map_response_status(
                Some(OpenAiResponseStatus::Incomplete),
                Some(&details),
                false
            ),
            FinishReason::ContentFilter
        );
    }

    #[test]
    fn map_response_status_maps_completed_with_tool_calls() {
        assert_eq!(
            map_response_status(Some(OpenAiResponseStatus::Completed), None, true),
            FinishReason::ToolCalls
        );
    }

    #[test]
    fn map_response_status_maps_failed_to_error() {
        assert_eq!(
            map_response_status(Some(OpenAiResponseStatus::Failed), None, false),
            FinishReason::Error
        );
        assert_eq!(
            map_response_status(Some(OpenAiResponseStatus::Cancelled), None, false),
            FinishReason::Error
        );
    }

    #[test]
    fn map_openai_usage_preserves_reasoning_token_breakdown() {
        let usage = OpenAiUsage {
            input_tokens: 10,
            output_tokens: 20,
            input_tokens_details: Some(OpenAiInputTokenDetails {
                cached_tokens: Some(3),
            }),
            output_tokens_details: Some(OpenAiOutputTokenDetails {
                reasoning_tokens: Some(7),
            }),
        };

        let mapped = map_openai_usage(usage);

        assert_eq!(mapped.input_tokens, 10);
        assert_eq!(mapped.output_tokens, 20);
        assert_eq!(mapped.cached_input_tokens, Some(3));
        assert_eq!(mapped.reasoning_tokens, Some(7));
    }
}
