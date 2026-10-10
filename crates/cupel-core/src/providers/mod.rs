//! Protocol adapters ("providers").
//!
//! Each submodule translates between the unified types in [`crate::types`]
//! and one vendor wire protocol:
//!
//! - [`anthropic`]: Anthropic Messages API (SSE)
//! - [`openai_responses`]: `OpenAI` Responses API (SSE)
//! - [`openai_completions`]: `OpenAI` Chat Completions API (SSE), the
//!   protocol most "OpenAI-compatible" vendors (Fireworks, OpenRouter, ...) speak
//! - [`openai_codex_responses`]: the ChatGPT Codex backend's Responses
//!   dialect (SSE), driven by ChatGPT OAuth tokens instead of API keys
//! - [`bedrock`]: AWS Bedrock `ConverseStream` (binary event stream via the
//!   official AWS SDK)
//!
//! All providers follow the same stream functions:
//! `stream()` returns immediately; the network work happens on a spawned
//! Tokio task; *every* failure after that point is delivered as an `Error`
//! event on the stream, never as a panic.

pub mod anthropic;
pub mod bedrock;
pub mod openai_codex_responses;
pub mod openai_completions;
pub mod openai_responses;

use crate::error::{InferenceError, Result};
use crate::event_stream::{AssistantMessageStream, EventSink, assistant_message_channel};
use crate::model::{clamp_thinking_level, clamp_thinking_level_with};
use crate::types::{
    AssistantMessage, ErrorKind, Model, ModelThinkingLevel, StopReason, StreamOptions,
    ThinkingLevel, Usage, now_ms,
};

pub(crate) const CONTENT_FILTER_MESSAGE: &str =
    "The provider's content filter stopped the response. Partial output may be incomplete.";

/// Parse `model.compat` into an adapter's knob struct one key at a time.
///
/// A key whose value has the wrong type costs only that knob, which keeps its
/// default; every other key still applies. Parsing the whole object at once
/// would reset all knobs for one typo. `T` needs `#[serde(default)]` so a
/// single key parses on its own; unknown keys are ignored as before.
/// Returns the knobs and one problem line per rejected key.
pub(crate) fn parse_compat<T>(model: &Model) -> (T, Vec<String>)
where
    T: serde::de::DeserializeOwned + Default,
{
    let Some(compat) = &model.compat else {
        return (T::default(), Vec::new());
    };
    let Some(entries) = compat.as_object() else {
        return (
            T::default(),
            vec!["compat must be a JSON object".to_string()],
        );
    };
    let mut accepted = serde_json::Map::new();
    let mut problems = Vec::new();
    for (key, value) in entries {
        let single: serde_json::Map<String, serde_json::Value> =
            [(key.clone(), value.clone())].into_iter().collect();
        match serde_json::from_value::<T>(serde_json::Value::Object(single)) {
            Ok(_) => {
                accepted.insert(key.clone(), value.clone());
            }
            Err(error) => problems.push(format!("{key}: {error}")),
        }
    }
    // Every accepted key parsed on its own, so together they parse too.
    let knobs = serde_json::from_value(serde_json::Value::Object(accepted)).unwrap_or_default();
    (knobs, problems)
}

/// [`parse_compat`] for a request. The catalog loader already showed the
/// problems to the user (see [`Provider::compat_problems`](crate::provider::Provider::compat_problems)),
/// so here they only reach the log.
pub(crate) fn compat<T>(model: &Model) -> T
where
    T: serde::de::DeserializeOwned + Default,
{
    let (knobs, problems) = parse_compat(model);
    log_compat_problems(model, &problems);
    knobs
}

pub(crate) fn log_compat_problems(model: &Model, problems: &[String]) {
    for problem in problems {
        tracing::warn!(
            model = %model.id,
            provider = %model.provider.as_str(),
            "ignoring compat setting {problem}"
        );
    }
}

/// Shared producer/consumer split and in-band worker error contract.
pub(crate) fn spawn_provider_stream<F>(
    model: &Model,
    worker: impl FnOnce(Model, EventSink) -> F + Send + 'static,
) -> AssistantMessageStream
where
    F: core::future::Future<Output = Result<()>> + Send + 'static,
{
    let (stream, sink) = assistant_message_channel();
    let model = model.clone();
    tokio::spawn(async move {
        if let Err(error) = worker(model.clone(), sink.clone()).await {
            tracing::warn!(error = %error, "provider request failed");
            let message = error_message(&model, &error);
            let _ = sink.error(message.stop_reason, message);
        }
    });
    stream
}

/// Send a request and preserve HTTP status/body errors. Cancellation also applies
/// while reading an error body; a missing body never hides the known status.
pub(crate) async fn send_request(
    request: reqwest::RequestBuilder,
    options: &StreamOptions,
) -> Result<reqwest::Response> {
    let response = with_cancel(options, request.send()).await??;
    check_http_response(response, options).await
}

async fn check_http_response(
    response: reqwest::Response,
    options: &StreamOptions,
) -> Result<reqwest::Response> {
    let status = response.status();
    if status.is_success() {
        return Ok(response);
    }
    let body = with_cancel(options, response.text())
        .await?
        .unwrap_or_default();
    Err(InferenceError::ApiStatus {
        status: status.as_u16(),
        body,
    })
}

/// Protocol-native fallback names. Model metadata is still authoritative for
/// clamping and remapping; old Bedrock Claude models have no native xhigh.
#[derive(Clone, Copy)]
pub(crate) enum EffortStyle {
    OpenAi,
    Claude { native_xhigh: bool },
}

#[must_use]
pub(crate) fn thinking_effort(
    model: &Model,
    requested: Option<ThinkingLevel>,
    style: EffortStyle,
) -> Option<String> {
    let requested = match (requested?, style) {
        (
            ThinkingLevel::XHigh,
            EffortStyle::Claude {
                native_xhigh: false,
            },
        ) => ModelThinkingLevel::High,
        (level, _) => ModelThinkingLevel::from(level),
    };
    let level = match style {
        EffortStyle::OpenAi => clamp_thinking_level(model, requested),
        EffortStyle::Claude { native_xhigh } => {
            clamp_thinking_level_with(model, requested, |level| match level {
                ModelThinkingLevel::XHigh => native_xhigh,
                // Claude has no native "minimal" effort, but an explicit alias is valid.
                ModelThinkingLevel::Minimal => model
                    .thinking_level_map
                    .as_ref()
                    .is_some_and(|map| matches!(map.get("minimal"), Some(Some(_)))),
                _ => true,
            })
        }
    };
    if level == ModelThinkingLevel::Off {
        return None;
    }
    Some(
        model
            .thinking_level_map
            .as_ref()
            .and_then(|map| map.get(level.as_str()))
            .and_then(Option::as_ref)
            .cloned()
            .unwrap_or_else(|| level.as_str().to_string()),
    )
}

/// Explicit off never clamps up into enabled reasoning. A null override means
/// omission, a named override wins, and an absent entry uses the wire default.
#[must_use]
pub(crate) fn off_effort<'a>(model: &'a Model, default: Option<&'a str>) -> Option<&'a str> {
    match model
        .thinking_level_map
        .as_ref()
        .and_then(|map| map.get("off"))
    {
        Some(mapped) => mapped.as_deref(),
        None => default,
    }
}

/// Anthropic Messages and Bedrock share the same 64-character tool-call ids.
pub(crate) fn normalize_anthropic_tool_call_id(
    id: &str,
    _model: &Model,
    _source: &AssistantMessage,
) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect()
}

/// Build the skeleton assistant message a provider accumulates into while
/// streaming. Every provider starts from this same shape.
#[must_use]
pub(crate) fn new_output_message(model: &Model) -> AssistantMessage {
    AssistantMessage {
        content: Vec::new(),
        api: model.api.clone(),
        provider: model.provider.clone(),
        model: model.id.clone(),
        response_model: None,
        response_id: None,
        usage: Usage::default(),
        stop_reason: StopReason::Stop,
        error_message: None,
        error_kind: None,
        timestamp: now_ms(),
    }
}

/// Build the minimal error message emitted when a provider task fails before
/// (or instead of) producing a terminal event.
#[must_use]
pub(crate) fn error_message(model: &Model, error: &InferenceError) -> AssistantMessage {
    let kind = error.kind();
    AssistantMessage {
        stop_reason: if kind == ErrorKind::Aborted {
            StopReason::Aborted
        } else {
            StopReason::Error
        },
        error_message: Some(error.to_string()),
        error_kind: Some(kind),
        ..new_output_message(model)
    }
}

/// Finish a protocol-complete response without discarding its content or usage.
/// Mapped error stops are terminal Error events, not worker failures that the
/// outer wrapper would replace with an empty error message.
pub(crate) fn finish_output(mut output: AssistantMessage, sink: &EventSink) {
    let reason = output.stop_reason;
    if matches!(reason, StopReason::Error | StopReason::Aborted) {
        output
            .error_message
            .get_or_insert_with(|| "The provider stopped generation with an error".to_string());
        output
            .error_kind
            .get_or_insert(if reason == StopReason::Aborted {
                ErrorKind::Aborted
            } else {
                ErrorKind::Provider
            });
    }
    log_completion(&output);
    let _ = match reason {
        StopReason::Error | StopReason::Aborted => sink.error(reason, output),
        _ => sink.done(reason, output),
    };
}

/// Await a future, racing it against the caller's cancellation token.
///
/// This is the Rust analogue of passing an `AbortSignal` into fetch: instead
/// of the signal being threaded through the HTTP client, we `select!` between
/// "the work" and "cancellation" at every await point that can block.
pub(crate) async fn with_cancel<T>(
    options: &StreamOptions,
    fut: impl core::future::Future<Output = T>,
) -> Result<T> {
    match &options.signal {
        Some(token) => {
            tokio::select! {
                // `biased` checks cancellation first on every poll, so a
                // cancelled request never completes "by accident".
                biased;
                () = token.cancelled() => Err(crate::error::InferenceError::Aborted),
                value = fut => Ok(value),
            }
        }
        None => Ok(fut.await),
    }
}

/// Log the terminal outcome of one provider request. This is the
/// observability record for cost accounting: one INFO line per request with
/// exact token counts and dollars. Request duration comes from the enclosing
/// provider span (emitted on span close when the subscriber enables span
/// events), so it isn't duplicated here.
pub(crate) fn log_completion(message: &AssistantMessage) {
    tracing::info!(
        stop_reason = ?message.stop_reason,
        input_tokens = message.usage.input,
        output_tokens = message.usage.output,
        cache_read_tokens = message.usage.cache_read,
        cache_write_tokens = message.usage.cache_write,
        cost_usd = message.usage.cost.total,
        response_id = message.response_id.as_deref().unwrap_or(""),
        "provider request complete"
    );
}

/// Apply model-level then option-level custom headers to a request builder
/// (option-level wins).
pub(crate) fn apply_custom_headers(
    mut req: reqwest::RequestBuilder,
    model: &Model,
    options: &StreamOptions,
) -> reqwest::RequestBuilder {
    if let Some(headers) = &model.headers {
        for (key, value) in headers {
            req = req.header(key.as_str(), value.as_str());
        }
    }
    if let Some(headers) = &options.headers {
        for (key, value) in headers {
            req = req.header(key.as_str(), value.as_str());
        }
    }
    req
}

#[cfg(test)]
mod tests {
    use crate::error::{InferenceError, MessageStreamError};
    use crate::event_stream::assistant_message_channel;
    use crate::providers::{
        EffortStyle, check_http_response, error_message, finish_output, new_output_message,
        normalize_anthropic_tool_call_id, off_effort, send_request, spawn_provider_stream,
        thinking_effort,
    };
    use crate::types::{
        AssistantContent, AssistantMessage, ErrorKind, Model, StopReason, StreamOptions,
        TextContent, ThinkingLevel,
    };

    fn reasoning_model() -> Model {
        let mut model = crate::catalog::builtin_models().remove(0);
        model.thinking_level_map = None;
        model.reasoning = true;
        model
    }

    #[test]
    fn effort_scales_preserve_native_top_levels_and_legacy_claude_limits() {
        let model = reasoning_model();
        for (level, openai, claude, legacy) in [
            (ThinkingLevel::Minimal, "minimal", "low", "low"),
            (ThinkingLevel::Low, "low", "low", "low"),
            (ThinkingLevel::Medium, "medium", "medium", "medium"),
            (ThinkingLevel::High, "high", "high", "high"),
            (ThinkingLevel::XHigh, "xhigh", "xhigh", "high"),
            (ThinkingLevel::Max, "max", "max", "max"),
        ] {
            for (style, expected) in [
                (EffortStyle::OpenAi, openai),
                (EffortStyle::Claude { native_xhigh: true }, claude),
                (
                    EffortStyle::Claude {
                        native_xhigh: false,
                    },
                    legacy,
                ),
            ] {
                assert_eq!(
                    thinking_effort(&model, Some(level), style).as_deref(),
                    Some(expected)
                );
                assert!(thinking_effort(&model, None, style).is_none());
            }
        }
    }

    #[test]
    fn effort_clamps_then_applies_aliases_and_respects_wire_constraints() {
        let mut model = reasoning_model();
        model.thinking_level_map = Some(
            [
                ("minimal".to_string(), None),
                ("low".to_string(), None),
                ("medium".to_string(), Some("balanced".to_string())),
                ("high".to_string(), None),
                ("max".to_string(), None),
            ]
            .into_iter()
            .collect(),
        );
        for style in [
            EffortStyle::OpenAi,
            EffortStyle::Claude { native_xhigh: true },
            EffortStyle::Claude {
                native_xhigh: false,
            },
        ] {
            assert_eq!(
                thinking_effort(&model, Some(ThinkingLevel::Minimal), style).as_deref(),
                Some("balanced")
            );
        }
        assert_eq!(
            thinking_effort(
                &model,
                Some(ThinkingLevel::Max),
                EffortStyle::Claude {
                    native_xhigh: false
                }
            )
            .as_deref(),
            Some("balanced")
        );
        assert_eq!(
            thinking_effort(&model, Some(ThinkingLevel::Max), EffortStyle::OpenAi).as_deref(),
            Some("xhigh")
        );
        model.reasoning = false;
        assert!(thinking_effort(&model, Some(ThinkingLevel::High), EffortStyle::OpenAi).is_none());
    }

    #[test]
    fn off_overrides_are_shared_but_wire_defaults_remain_explicit() {
        let mut model = reasoning_model();
        assert_eq!(off_effort(&model, Some("disabled")), Some("disabled"));
        assert_eq!(off_effort(&model, Some("none")), Some("none"));
        assert_eq!(off_effort(&model, None), None);
        for mapped in [
            None,
            Some("between_tools".to_string()),
            Some("none".to_string()),
        ] {
            model.thinking_level_map =
                Some([("off".to_string(), mapped.clone())].into_iter().collect());
            for default in [None, Some("none"), Some("disabled")] {
                assert_eq!(off_effort(&model, default), mapped.as_deref());
            }
            assert!(thinking_effort(&model, None, EffortStyle::OpenAi).is_none());
        }
    }

    #[test]
    fn anthropic_id_normalization_is_ascii_and_capped_without_trimming() {
        let model = reasoning_model();
        let source = new_output_message(&model);
        assert_eq!(
            normalize_anthropic_tool_call_id("call-1|雪 ", &model, &source),
            "call-1___"
        );
        assert_eq!(
            normalize_anthropic_tool_call_id(&"x".repeat(100), &model, &source),
            "x".repeat(64)
        );
    }

    #[tokio::test]
    async fn spawned_workers_emit_typed_errors_or_preserve_terminal_output() {
        let model = reasoning_model();
        for error in [
            InferenceError::Aborted,
            InferenceError::MissingApiKey("mock".to_string()),
            InferenceError::ApiStatus {
                status: 429,
                body: "slow down".to_string(),
            },
        ] {
            let mut expected = error_message(&model, &error);
            let stream = spawn_provider_stream(&model, move |_, _| async move { Err(error) });
            let MessageStreamError::ProviderError { reason, message } =
                stream.result().await.unwrap_err()
            else {
                panic!("worker errors must produce a terminal Error event");
            };
            assert_eq!(reason, expected.stop_reason);
            expected.timestamp = message.timestamp;
            assert_eq!(*message, expected);
        }
        let mut expected = new_output_message(&model);
        expected
            .content
            .push(AssistantContent::Text(TextContent::plain("complete")));
        let output = expected.clone();
        let stream = spawn_provider_stream(&model, move |_, sink| async move {
            finish_output(output, &sink);
            Ok(())
        });
        assert_eq!(stream.result().await.unwrap(), expected);
    }

    async fn request_for_response(response: String) -> reqwest::RequestBuilder {
        use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut socket = tokio::io::BufReader::new(socket);
            let mut line = String::new();
            while socket.read_line(&mut line).await.unwrap() > 0 {
                if line == "\r\n" {
                    break;
                }
                line.clear();
            }
            socket
                .get_mut()
                .write_all(response.as_bytes())
                .await
                .unwrap();
        });
        reqwest::Client::builder()
            .no_proxy()
            .timeout(std::time::Duration::from_secs(2))
            .build()
            .unwrap()
            .get(format!("http://{address}"))
    }

    #[tokio::test]
    async fn http_status_errors_keep_the_body_and_do_not_match_body_digits() {
        let body = "requested 500429 tokens";
        for status in [200, 400, 401, 429, 500] {
            let request = request_for_response(format!(
                "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            ))
            .await;
            let result = send_request(request, &StreamOptions::default()).await;
            if status == 200 {
                assert_eq!(result.unwrap().text().await.unwrap(), body);
            } else {
                let error = result.unwrap_err();
                assert_eq!(error.kind(), ErrorKind::HttpStatus { status });
                let InferenceError::ApiStatus { body: actual, .. } = &error else {
                    panic!("HTTP error");
                };
                assert_eq!(actual, body);
                let message = error_message(&reasoning_model(), &error);
                assert_eq!(
                    crate::retry::is_retryable_assistant_error(&message),
                    status == 429 || status == 500
                );
            }
        }
    }

    #[tokio::test]
    async fn responses_streams_that_end_early_are_retryable_transport_errors() {
        // A 200 whose event stream stops before `response.completed`: the
        // connection broke, so the turn is worth sending again.
        let body = "data: {\"type\":\"response.created\",\"response\":{\"id\":\"r\"}}\n\n";
        let request = request_for_response(format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        ))
        .await;
        let response = request.send().await.unwrap();
        let model = reasoning_model();
        let (_stream, sink) = assistant_message_channel();
        let error = crate::providers::openai_responses::process_response_stream(
            response,
            &model,
            &StreamOptions::default(),
            &sink,
        )
        .await
        .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::Transport, "{error}");
        assert!(crate::retry::is_retryable_assistant_error(&error_message(
            &model, &error
        )));
    }

    #[derive(Debug, Default, PartialEq, serde::Deserialize)]
    #[serde(rename_all = "camelCase", default)]
    struct Knobs {
        requires_api_key: Option<bool>,
        supports_store: Option<bool>,
    }

    #[test]
    fn compat_parsing_keeps_every_key_that_fits() {
        let mut model = reasoning_model();
        model.compat = Some(serde_json::json!({
            "requiresApiKey": false,
            "supportsStore": "false",
            "somethingElse": 1,
        }));
        let (knobs, problems) = crate::providers::parse_compat::<Knobs>(&model);
        // One typo costs only its own knob; unknown keys stay ignored.
        assert_eq!(knobs.requires_api_key, Some(false));
        assert_eq!(knobs.supports_store, None);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].starts_with("supportsStore: "), "{problems:?}");

        model.compat = Some(serde_json::json!(["not", "an", "object"]));
        let (knobs, problems) = crate::providers::parse_compat::<Knobs>(&model);
        assert_eq!(knobs, Knobs::default());
        assert_eq!(problems, ["compat must be a JSON object"]);

        model.compat = None;
        assert_eq!(
            crate::providers::parse_compat::<Knobs>(&model),
            (Knobs::default(), Vec::new())
        );
    }

    #[tokio::test]
    async fn truncated_http_error_bodies_do_not_hide_the_status() {
        let request = request_for_response(
            "HTTP/1.1 400 Bad Request\r\nContent-Length: 100\r\nConnection: close\r\n\r\nshort"
                .to_string(),
        )
        .await;
        let error = send_request(request, &StreamOptions::default())
            .await
            .unwrap_err();
        assert_eq!(error.kind(), ErrorKind::HttpStatus { status: 400 });
    }

    #[tokio::test]
    async fn error_body_reads_honor_cancellation() {
        let request = request_for_response(
            "HTTP/1.1 500 Error\r\nContent-Length: 4\r\nConnection: close\r\n\r\nbody".to_string(),
        )
        .await;
        let response = request.send().await.unwrap();
        let signal = tokio_util::sync::CancellationToken::new();
        signal.cancel();
        let options = StreamOptions {
            signal: Some(signal),
            ..StreamOptions::default()
        };
        assert!(matches!(
            check_http_response(response, &options).await,
            Err(InferenceError::Aborted)
        ));
    }

    #[test]
    fn worker_errors_keep_their_category_and_display_text() {
        let model = crate::catalog::builtin_models().remove(0);
        for error in [
            InferenceError::ApiStatus {
                status: 400,
                body: "request id 429500".into(),
            },
            InferenceError::ApiStatus {
                status: 503,
                body: "opaque".into(),
            },
            InferenceError::MissingApiKey("provider".into()),
            InferenceError::Aborted,
            InferenceError::Other("ThrottlingException".into()),
        ] {
            let message = error_message(&model, &error);
            assert_eq!(message.error_kind, Some(error.kind()));
            assert_eq!(message.error_message, Some(error.to_string()));
            assert_eq!(
                message.stop_reason == StopReason::Aborted,
                error.kind() == ErrorKind::Aborted
            );
        }
    }

    #[test]
    fn error_categories_round_trip_and_old_messages_remain_readable() {
        let model = crate::catalog::builtin_models().remove(0);
        let mut message = new_output_message(&model);
        let legacy = serde_json::to_value(&message).unwrap();
        assert!(legacy.get("errorKind").is_none());
        let restored: AssistantMessage = serde_json::from_value(legacy).unwrap();
        assert_eq!(restored.error_kind, None);

        for kind in [
            ErrorKind::HttpStatus { status: 429 },
            ErrorKind::Transport,
            ErrorKind::Aborted,
            ErrorKind::Config,
            ErrorKind::Provider,
        ] {
            message.error_kind = Some(kind);
            let json = serde_json::to_value(&message).unwrap();
            let restored: AssistantMessage = serde_json::from_value(json).unwrap();
            assert_eq!(restored, message);
        }
    }

    #[tokio::test]
    async fn terminal_stops_preserve_the_accumulated_message() {
        let model = crate::catalog::builtin_models().into_iter().next().unwrap();
        for reason in [
            StopReason::Stop,
            StopReason::Length,
            StopReason::ToolUse,
            StopReason::Error,
            StopReason::Aborted,
        ] {
            let mut output = new_output_message(&model);
            output.stop_reason = reason;
            output.content = vec![AssistantContent::Text(TextContent::plain("partial output"))];
            output.response_id = Some("response-id".into());
            output.response_model = Some("served-model".into());
            output.usage.input = 100;
            output.usage.output = 12;
            output.usage.cache_read = 30;
            output.usage.total_tokens = 142;
            output.usage.cost.total = 0.25;
            let failed = matches!(reason, StopReason::Error | StopReason::Aborted);
            if failed {
                output.error_message = Some("provider stop explanation".into());
                output.error_kind = Some(if reason == StopReason::Aborted {
                    ErrorKind::Aborted
                } else {
                    ErrorKind::Provider
                });
            }

            let (stream, sink) = assistant_message_channel();
            finish_output(output.clone(), &sink);
            drop(sink);
            let actual = match stream.result().await {
                Ok(message) => {
                    assert!(!failed, "error stops must not be emitted as Done");
                    message
                }
                Err(MessageStreamError::ProviderError {
                    reason: reported,
                    message,
                }) => {
                    assert!(failed);
                    assert_eq!(reported, reason);
                    *message
                }
                Err(error) => panic!("unexpected stream error: {error}"),
            };
            assert_eq!(actual, output);
        }
    }
}
