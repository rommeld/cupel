use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::provider::Provider as _;
use crate::providers::google::{GoogleCompat, GoogleProvider, build_request_body};
use crate::types::{
    Api, AssistantContent, AssistantMessage, AssistantMessageEvent, Context, ErrorKind,
    ImageContent, InputModality, Message, Model, ModelCost, Provider, StopReason, StreamOptions,
    TextContent, ThinkingLevel, Tool, ToolCall, ToolResultContent, ToolResultMessage, UserContent,
    UserContentBody, UserMessage,
};

fn fixture_model() -> Model {
    Model {
        id: "gemini-4-carbon".into(),
        name: "Gemini 4 Carbon".into(),
        api: Api::from(Api::GOOGLE_GENERATIVE_AI),
        provider: Provider::from(Provider::GOOGLE),
        base_url: "http://127.0.0.1:9/v1beta".into(),
        reasoning: true,
        thinking_level_map: Some(
            [
                ("off", None),
                ("minimal", None),
                ("low", Some("low")),
                ("medium", Some("medium")),
                ("high", Some("high")),
                ("xhigh", None),
                ("max", None),
            ]
            .into_iter()
            .map(|(key, value)| (key.to_string(), value.map(str::to_string)))
            .collect(),
        ),
        input: vec![InputModality::Text, InputModality::Image],
        cost: ModelCost {
            input: 2.0,
            output: 10.0,
            cached_read: 0.1,
            ..ModelCost::default()
        },
        context_window: 1_048_576,
        max_context_window: None,
        max_tokens: 16_384,
        headers: None,
        compat: Some(json!({"supportsTemperature": false})),
    }
}

fn context() -> Context {
    Context {
        system_prompt: Some("Use the available tools.".into()),
        messages: vec![Message::User(UserMessage {
            content: UserContentBody::Text("Read both files.".into()),
            timestamp: 0,
        })],
        tools: Some(vec![Tool {
            name: "read".into(),
            description: "Read a file".into(),
            parameters: json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"],
                "additionalProperties": false,
            }),
        }]),
    }
}

fn options() -> StreamOptions {
    StreamOptions {
        api_key: Some("fixture-google-key".into()),
        timeout_ms: Some(5_000),
        reasoning: Some(ThinkingLevel::Medium),
        temperature: Some(0.2),
        ..StreamOptions::default()
    }
}

struct CapturedRequest {
    request_line: String,
    headers: BTreeMap<String, String>,
    body: Value,
}

async fn capture_request(
    socket: tokio::net::TcpStream,
) -> (CapturedRequest, tokio::net::tcp::OwnedWriteHalf) {
    use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _};
    let (reader, writer) = socket.into_split();
    let mut reader = tokio::io::BufReader::new(reader);
    let mut request_line = String::new();
    reader.read_line(&mut request_line).await.unwrap();
    let mut headers = BTreeMap::new();
    loop {
        let mut line = String::new();
        assert!(reader.read_line(&mut line).await.unwrap() > 0);
        if line == "\r\n" {
            break;
        }
        let (name, value) = line.split_once(':').unwrap();
        headers.insert(name.to_ascii_lowercase(), value.trim().to_string());
    }
    let length = headers["content-length"].parse().unwrap();
    let mut body = vec![0; length];
    reader.read_exact(&mut body).await.unwrap();
    (
        CapturedRequest {
            request_line,
            headers,
            body: serde_json::from_slice(&body).unwrap(),
        },
        writer,
    )
}

async fn server(replies: Vec<(u16, String)>) -> (Model, JoinHandle<Vec<CapturedRequest>>) {
    use tokio::io::AsyncWriteExt as _;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let mut requests = Vec::new();
        for (status, body) in replies {
            let (socket, _) = listener.accept().await.unwrap();
            let (request, mut writer) = capture_request(socket).await;
            requests.push(request);
            let response = format!(
                "HTTP/1.1 {status} Fixture\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len(),
            );
            writer.write_all(response.as_bytes()).await.unwrap();
            writer.shutdown().await.unwrap();
        }
        requests
    });
    let mut model = fixture_model();
    model.base_url = format!("http://{address}/v1beta/");
    (model, task)
}

fn sse(chunks: &[Value]) -> String {
    use core::fmt::Write as _;
    let mut body = String::new();
    for chunk in chunks {
        writeln!(body, "data: {chunk}\n").unwrap();
    }
    body
}

async fn events(
    model: &Model,
    context: Context,
    options: StreamOptions,
) -> Vec<AssistantMessageEvent> {
    use futures_util::StreamExt as _;
    tokio::time::timeout(
        Duration::from_secs(6),
        GoogleProvider::new()
            .stream(model, context, options)
            .collect(),
    )
    .await
    .expect("provider must terminate its stream")
}

fn terminal(events: &[AssistantMessageEvent]) -> &AssistantMessage {
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(
                event,
                AssistantMessageEvent::Done { .. } | AssistantMessageEvent::Error { .. }
            ))
            .count(),
        1,
        "each stream must have exactly one terminal event",
    );
    match events.last().unwrap() {
        AssistantMessageEvent::Done { message, .. } => message,
        AssistantMessageEvent::Error { error, .. } => error,
        other => panic!("expected terminal event, got {other:?}"),
    }
}

#[tokio::test]
async fn streamed_parallel_calls_survive_saved_history_and_wire_roundtrip() {
    let first = sse(&[
        json!({"responseId": "response-1", "modelVersion": "gemini-4-carbon-001",
            "candidates": [{"content": {"parts": [{"thought": true, "text": "Check "}]}}]}),
        json!({"candidates": [{"content": {"parts": [{"thought": true, "text": "both files", "thoughtSignature": "thinking-signature"}]}}]}),
        json!({"candidates": [{"content": {"parts": [
            {"text": "Reading files.", "thoughtSignature": "text-signature"},
            {"functionCall": {"id": "google-call-a", "name": "read", "args": {"path": "a.rs"}}, "thoughtSignature": "call-signature-a"},
            {"functionCall": {"id": "google-call-b", "name": "read", "args": {"path": "b.rs"}}}
        ]}, "finishReason": "STOP"}]}),
        json!({"usageMetadata": {"promptTokenCount": 100, "cachedContentTokenCount": 40,
            "candidatesTokenCount": 20, "thoughtsTokenCount": 10, "totalTokenCount": 130}}),
    ]);
    let second = sse(&[
        json!({"candidates": [{"content": {"parts": [{"text": "Done."}]}, "finishReason": "STOP"}]}),
    ]);
    let (model, task) = server(vec![(200, first), (200, second)]).await;
    let first_events = events(&model, context(), options()).await;
    let message = terminal(&first_events);
    assert_eq!(message.stop_reason, StopReason::ToolUse);
    assert_eq!(message.response_id.as_deref(), Some("response-1"));
    assert_eq!(
        message.response_model.as_deref(),
        Some("gemini-4-carbon-001")
    );
    assert_eq!(
        (
            message.usage.input,
            message.usage.cache_read,
            message.usage.output
        ),
        (60, 40, 30)
    );
    assert_eq!(message.usage.reasoning, Some(10));
    assert_eq!(message.usage.total_tokens, 130);
    assert!((message.usage.cost.total - 0.000_424).abs() < 1e-12);
    assert!(first_events.iter().any(|event| matches!(event, AssistantMessageEvent::ThinkingDelta { delta, .. } if delta == "Check ")));
    assert_eq!(
        first_events
            .iter()
            .filter(|event| matches!(event, AssistantMessageEvent::ToolCallEnd { .. }))
            .count(),
        2
    );

    let saved = serde_json::to_string(message).unwrap();
    let restored: AssistantMessage = serde_json::from_str(&saved).unwrap();
    let mut next = context();
    next.messages.push(Message::Assistant(restored));
    next.messages.push(Message::ToolResult(ToolResultMessage {
        tool_call_id: "google-call-a".into(),
        tool_name: "read".into(),
        content: vec![
            ToolResultContent::Text(TextContent::plain("file A")),
            ToolResultContent::Image(ImageContent {
                data: "aW1hZ2U=".into(),
                mime_type: "image/png".into(),
            }),
        ],
        details: None,
        is_error: false,
        timestamp: 0,
    }));
    next.messages.push(Message::ToolResult(ToolResultMessage {
        tool_call_id: "google-call-b".into(),
        tool_name: "read".into(),
        content: vec![ToolResultContent::Text(TextContent::plain("file missing"))],
        details: None,
        is_error: true,
        timestamp: 0,
    }));
    assert_eq!(
        terminal(&events(&model, next, options()).await).stop_reason,
        StopReason::Stop
    );
    let requests = task.await.unwrap();
    assert_eq!(requests.len(), 2);
    for request in &requests {
        assert_eq!(
            request.request_line,
            "POST /v1beta/models/gemini-4-carbon:streamGenerateContent?alt=sse HTTP/1.1\r\n"
        );
        assert_eq!(request.headers["x-goog-api-key"], "fixture-google-key");
        assert!(!request.request_line.contains("fixture-google-key"));
        assert_eq!(request.headers["accept"], "text/event-stream");
        assert_eq!(
            request.body["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            "MEDIUM"
        );
        assert!(
            request.body["generationConfig"]
                .get("temperature")
                .is_none()
        );
        assert_eq!(
            request.body["tools"][0]["functionDeclarations"][0]["parametersJsonSchema"],
            context().tools.unwrap()[0].parameters
        );
    }
    let replay = &requests[1].body["contents"][1];
    assert_eq!(replay["role"], "model");
    assert_eq!(
        replay["parts"][0],
        json!({"text": "Check ", "thought": true})
    );
    assert_eq!(
        replay["parts"][1],
        json!({"text": "both files", "thought": true, "thoughtSignature": "thinking-signature"})
    );
    assert_eq!(replay["parts"][2]["thoughtSignature"], "text-signature");
    assert_eq!(replay["parts"][3]["thoughtSignature"], "call-signature-a");
    assert!(replay["parts"][4].get("thoughtSignature").is_none());
    let results = &requests[1].body["contents"][2]["parts"];
    assert_eq!(results.as_array().unwrap().len(), 2);
    assert_eq!(results[0]["functionResponse"]["id"], "google-call-a");
    assert_eq!(
        results[0]["functionResponse"]["response"]["output"],
        "file A"
    );
    assert_eq!(
        results[0]["functionResponse"]["parts"][0]["inlineData"]["data"],
        "aW1hZ2U="
    );
    assert_eq!(results[1]["functionResponse"]["id"], "google-call-b");
    assert_eq!(
        results[1]["functionResponse"]["response"]["error"],
        "file missing"
    );
}

#[tokio::test]
async fn final_signatures_and_empty_signed_parts_are_kept_after_finish_reason() {
    let (model, task) = server(vec![(200, sse(&[
        json!({"candidates": [{"content": {"parts": [{"text": "answer"}]}, "finishReason": "STOP"}]}),
        json!({"candidates": [{"content": {"parts": [{"text": "signed text", "thoughtSignature": "signed-text-signature"}]}}]}),
        json!({"candidates": [{"content": {"parts": [{"text": "", "thoughtSignature": "late-signature"}]}}]}),
        json!({"candidates": [{"content": {"parts": [{"text": "", "thoughtSignature": "empty-part-signature"}]}}]}),
    ]))]).await;
    let received = events(&model, context(), options()).await;
    let message = terminal(&received);
    assert_eq!(message.stop_reason, StopReason::Stop);
    assert_eq!(
        message.content,
        vec![
            AssistantContent::Text(TextContent {
                text: "answer".into(),
                text_signature: None,
            }),
            AssistantContent::Text(TextContent {
                text: "signed text".into(),
                text_signature: Some("signed-text-signature".into()),
            }),
            AssistantContent::Text(TextContent {
                text: String::new(),
                text_signature: Some("late-signature".into())
            }),
            AssistantContent::Text(TextContent {
                text: String::new(),
                text_signature: Some("empty-part-signature".into())
            }),
        ]
    );
    task.await.unwrap();
}

#[tokio::test]
async fn missing_call_ids_are_unique_for_parallel_same_name_calls_and_later_turns() {
    let body = sse(&[json!({"candidates": [{"content": {"parts": [
        {"functionCall": {"name": "read", "args": {"path": "a.rs"}}},
        {"functionCall": {"name": "read", "args": {"path": "b.rs"}}}
    ]}, "finishReason": "STOP"}]})]);
    let (model, task) = server(vec![(200, body.clone()), (200, body)]).await;
    let mut ids = Vec::new();
    for _ in 0..2 {
        let received = events(&model, context(), options()).await;
        let message = terminal(&received);
        assert_eq!(message.stop_reason, StopReason::ToolUse);
        for block in &message.content {
            let AssistantContent::ToolCall(call) = block else {
                panic!("expected call");
            };
            assert!(!call.id.is_empty());
            ids.push(call.id.clone());
        }
    }
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 4);
    task.await.unwrap();
}

#[tokio::test]
async fn finish_reasons_and_failures_retain_partial_output() {
    let cases = [
        (
            sse(&[
                json!({"candidates": [{"content": {"parts": [{"text": "partial"}]}, "finishReason": "MAX_TOKENS"}]}),
            ]),
            StopReason::Length,
            None,
        ),
        (
            sse(&[
                json!({"candidates": [{"content": {"parts": [{"text": "partial"}]}, "finishReason": "SAFETY"}]}),
            ]),
            StopReason::Error,
            Some(ErrorKind::Provider),
        ),
        (
            format!(
                "{}data: {{broken\n\n",
                sse(&[json!({"candidates": [{"content": {"parts": [{"text": "partial"}]}}]})])
            ),
            StopReason::Error,
            Some(ErrorKind::Transport),
        ),
        (
            sse(&[json!({"candidates": [{"content": {"parts": [{"text": "partial"}]}}]})]),
            StopReason::Error,
            Some(ErrorKind::Transport),
        ),
    ];
    for (body, reason, kind) in cases {
        let (model, task) = server(vec![(200, body)]).await;
        let received = events(&model, context(), options()).await;
        let message = terminal(&received);
        assert_eq!(message.stop_reason, reason);
        assert_eq!(message.error_kind, kind);
        assert_eq!(
            message.content,
            vec![AssistantContent::Text(TextContent::plain("partial"))]
        );
        task.await.unwrap();
    }
}

#[tokio::test]
async fn prompt_blocks_invalid_calls_and_http_failures_are_errors() {
    let cases = [
        (
            200,
            sse(&[json!({"promptFeedback": {"blockReason": "SAFETY"}})]),
            ErrorKind::Provider,
        ),
        (
            200,
            sse(&[
                json!({"candidates": [{"content": {"parts": [{"functionCall": {"name": "read", "args": "invalid"}}]}, "finishReason": "STOP"}]}),
            ]),
            ErrorKind::Provider,
        ),
        (
            429,
            json!({"error": {"code": 429, "message": "quota exceeded"}}).to_string(),
            ErrorKind::HttpStatus { status: 429 },
        ),
        (
            200,
            sse(&[json!({"error": {"code": 429, "message": "quota exceeded"}})]),
            ErrorKind::HttpStatus { status: 429 },
        ),
    ];
    for (status, body, kind) in cases {
        let (model, task) = server(vec![(status, body)]).await;
        let received = events(&model, context(), options()).await;
        let message = terminal(&received);
        assert_eq!(message.stop_reason, StopReason::Error);
        assert_eq!(message.error_kind, Some(kind));
        assert!(message.error_message.is_some());
        task.await.unwrap();
    }
}

#[tokio::test]
async fn missing_credentials_fail_without_a_request() {
    let received = events(&fixture_model(), context(), StreamOptions::default()).await;
    let message = terminal(&received);
    assert_eq!(message.error_kind, Some(ErrorKind::Config));
    assert!(message.error_message.as_deref().unwrap().contains("google"));
}

#[tokio::test]
async fn cancellation_interrupts_a_stream_and_preserves_text() {
    use futures_util::StreamExt as _;
    use tokio::io::AsyncWriteExt as _;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut model = fixture_model();
    model.base_url = format!("http://{}/v1beta", listener.local_addr().unwrap());
    let task = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let (_, mut writer) = capture_request(socket).await;
        writer
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
        writer
            .write_all(
                sse(&[json!({"candidates": [{"content": {"parts": [{"text": "partial"}]}}]})])
                    .as_bytes(),
            )
            .await
            .unwrap();
        std::future::pending::<()>().await;
    });
    let signal = CancellationToken::new();
    let mut options = options();
    options.signal = Some(signal.clone());
    let mut stream = GoogleProvider::new().stream(&model, context(), options);
    let received = tokio::time::timeout(Duration::from_secs(3), async {
        let mut received = Vec::new();
        while let Some(event) = stream.next().await {
            if matches!(event, AssistantMessageEvent::TextDelta { .. }) {
                signal.cancel();
            }
            received.push(event);
        }
        received
    })
    .await
    .unwrap();
    task.abort();
    let message = terminal(&received);
    assert_eq!(message.stop_reason, StopReason::Aborted);
    assert_eq!(message.error_kind, Some(ErrorKind::Aborted));
    assert_eq!(
        message.content,
        vec![AssistantContent::Text(TextContent::plain("partial"))]
    );
}

#[test]
fn imported_tool_history_uses_signature_fallback_only_on_first_call() {
    let source = fixture_model();
    let mut assistant = crate::providers::new_output_message(&source);
    assistant.stop_reason = StopReason::ToolUse;
    assistant.content = vec![
        AssistantContent::ToolCall(ToolCall {
            id: "call-first".into(),
            name: "read".into(),
            arguments: json!({"path": "a.rs"}),
            thought_signature: Some("signature-for-carbon-only".into()),
        }),
        AssistantContent::ToolCall(ToolCall {
            id: "call-second".into(),
            name: "read".into(),
            arguments: json!({"path": "b.rs"}),
            thought_signature: None,
        }),
    ];
    let mut context = context();
    context.messages.push(Message::Assistant(assistant));
    let mut target = source;
    target.id = "gemini-4-argon".into();
    let body = build_request_body(&target, &context, &options(), &GoogleCompat::default());
    let parts = &body["contents"][1]["parts"];
    assert_eq!(
        parts[0]["thoughtSignature"],
        "skip_thought_signature_validator"
    );
    assert!(parts[1].get("thoughtSignature").is_none());
    assert_eq!(parts[0]["functionCall"]["id"], "call-first");
    assert_eq!(parts[1]["functionCall"]["id"], "call-second");
    assert!(!body.to_string().contains("signature-for-carbon-only"));
}

#[test]
fn request_config_clamps_gemini_4_thinking_and_output_and_keeps_images() {
    let mut context = context();
    context.messages = vec![Message::User(UserMessage {
        content: UserContentBody::Blocks(vec![
            UserContent::Text(TextContent::plain("inspect")),
            UserContent::Image(ImageContent {
                data: "aW1hZ2U=".into(),
                mime_type: "image/png".into(),
            }),
        ]),
        timestamp: 0,
    })];
    for id in ["gemini-4-argon", "gemini-4-carbon"] {
        let mut model = fixture_model();
        model.id = id.into();
        for (reasoning, expected) in [
            (None, "LOW"),
            (Some(ThinkingLevel::Minimal), "LOW"),
            (Some(ThinkingLevel::Max), "HIGH"),
        ] {
            let mut options = options();
            options.reasoning = reasoning;
            options.max_tokens = Some(u64::MAX);
            let compat = GoogleCompat {
                supports_temperature: false,
            };
            let body = build_request_body(&model, &context, &options, &compat);
            assert_eq!(
                body["generationConfig"]["thinkingConfig"]["thinkingLevel"],
                expected
            );
            assert_eq!(
                body["generationConfig"]["maxOutputTokens"],
                model.max_tokens
            );
            assert!(body["generationConfig"].get("temperature").is_none());
            assert_eq!(
                body["contents"][0]["parts"][1]["inlineData"]["mimeType"],
                "image/png"
            );
        }
    }
}
