use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::{Value, json};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::provider::Provider as _;
use crate::providers::google::{GoogleCompat, GoogleProvider, build_request_body};
use crate::types::{
    Api, AssistantContent, AssistantMessage, AssistantMessageEvent, Context, CostTier, ErrorKind,
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
    scripted_server(
        replies
            .into_iter()
            .map(|(status, body)| (status, body, false))
            .collect(),
    )
    .await
}

/// Interrupted responses declare bytes they never send, forcing a body-read error
/// after the supplied complete SSE events have been received.
async fn scripted_server(
    replies: Vec<(u16, String, bool)>,
) -> (Model, JoinHandle<Vec<CapturedRequest>>) {
    use tokio::io::AsyncWriteExt as _;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let mut requests = Vec::new();
        for (status, body, interrupted) in replies {
            let (socket, _) = listener.accept().await.unwrap();
            let (request, mut writer) = capture_request(socket).await;
            requests.push(request);
            let response = format!(
                "HTTP/1.1 {status} Fixture\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len() + usize::from(interrupted),
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

fn long_decode_options() -> StreamOptions {
    StreamOptions {
        max_tokens: Some(200_000),
        ..options()
    }
}

fn text_deltas(events: &[AssistantMessageEvent]) -> String {
    events
        .iter()
        .filter_map(|event| match event {
            AssistantMessageEvent::TextDelta { delta, .. } => Some(delta.as_str()),
            _ => None,
        })
        .collect()
}

async fn completed_requests(task: JoinHandle<Vec<CapturedRequest>>) -> Vec<CapturedRequest> {
    tokio::time::timeout(Duration::from_secs(6), task)
        .await
        .expect("all expected requests must arrive")
        .unwrap()
}

/// Keep accepting until the client finishes so accidental extra attempts are counted.
async fn repeating_server(
    body: String,
    interrupted: bool,
) -> (Model, CancellationToken, JoinHandle<Vec<CapturedRequest>>) {
    use tokio::io::AsyncWriteExt as _;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut model = fixture_model();
    model.base_url = format!("http://{}/v1beta", listener.local_addr().unwrap());
    model.max_tokens = 1_048_576;
    let finished = CancellationToken::new();
    let stop = finished.clone();
    let task = tokio::spawn(async move {
        let mut requests = Vec::new();
        loop {
            let (socket, _) = tokio::select! {
                () = stop.cancelled() => break,
                result = listener.accept() => result.unwrap(),
            };
            let (request, mut writer) = capture_request(socket).await;
            requests.push(request);
            writer.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len() + usize::from(interrupted)).as_bytes()).await.unwrap();
            writer.shutdown().await.unwrap();
        }
        requests
    });
    (model, finished, task)
}

#[tokio::test]
async fn long_decode_emits_text_and_thinking_before_checkpoint_or_eof() {
    use futures_util::StreamExt as _;
    use tokio::io::AsyncWriteExt as _;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut model = fixture_model();
    model.base_url = format!("http://{}/v1beta", listener.local_addr().unwrap());
    model.max_tokens = 1_048_576;
    let (release, wait_for_deltas) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let prefix = sse(&[json!({"candidates": [{"content": {"parts": [
            {"thought": true, "text": "Live thought"}, {"text": "Live text"}
        ]}}]})]);
        let checkpoint = sse(&[
            json!({"candidates": [{"finishReason": "STOP", "continuationToken": "final-checkpoint"}]}),
        ]);
        let (socket, _) = listener.accept().await.unwrap();
        let (_, mut writer) = capture_request(socket).await;
        writer.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{prefix}", prefix.len() + checkpoint.len()).as_bytes()).await.unwrap();
        wait_for_deltas
            .await
            .expect("client must receive both deltas before checkpoint release");
        writer.write_all(checkpoint.as_bytes()).await.unwrap();
        writer.shutdown().await.unwrap();
    });
    let mut stream = GoogleProvider::new().stream(&model, context(), long_decode_options());
    let received = tokio::time::timeout(Duration::from_secs(3), async {
        let mut received = Vec::new();
        let mut release = Some(release);
        let mut text_seen = false;
        let mut thinking_seen = false;
        while let Some(event) = stream.next().await {
            text_seen |= matches!(&event, AssistantMessageEvent::TextDelta { delta, .. } if delta == "Live text");
            thinking_seen |= matches!(&event, AssistantMessageEvent::ThinkingDelta { delta, .. } if delta == "Live thought");
            if text_seen && thinking_seen
                && let Some(release) = release.take()
            {
                release.send(()).unwrap();
            }
            received.push(event);
        }
        assert!(text_seen && thinking_seen);
        received
    }).await.expect("deltas must arrive while the checkpoint and EOF are withheld");
    assert_eq!(terminal(&received).stop_reason, StopReason::Stop);
    task.await.unwrap();
}

#[tokio::test]
async fn long_decode_sums_hop_usage_without_counting_replayed_context_twice() {
    for id in ["gemini-4-argon", "gemini-4-carbon", "gemini-4-hydrogen"] {
        let first = sse(&[
            json!({"candidates": [{"content": {"parts": [{"thought": true, "text": "Plan "}]}}],
                "usageMetadata": {"promptTokenCount": 100, "cachedContentTokenCount": 40,
                    "thoughtsTokenCount": 10, "totalTokenCount": 110}}),
            json!({"candidates": [{"content": {"parts": [{"thought": true, "text": "carefully.", "thoughtSignature": "planning-signature"}]},
                "finishReason": "CONTINUATION", "continuationToken": "checkpoint-1"}]}),
            json!({"usageMetadata": {"promptTokenCount": 100, "cachedContentTokenCount": 40,
                "thoughtsTokenCount": 30, "totalTokenCount": 130}}),
        ]);
        let second = sse(&[
            json!({"candidates": [{"content": {"parts": [{"text": "Reading."}]}}],
                "usageMetadata": {"promptTokenCount": 130, "cachedContentTokenCount": 100,
                    "candidatesTokenCount": 8, "thoughtsTokenCount": 2, "totalTokenCount": 140}}),
            json!({"candidates": [{"content": {"parts": [
                {"functionCall": {"id": "retained-call", "name": "read", "args": {"path": "a.rs"}}, "thoughtSignature": "retained-signature"}
            ]}, "finishReason": "STOP"}]}),
            json!({"usageMetadata": {"promptTokenCount": 130, "cachedContentTokenCount": 100,
                "candidatesTokenCount": 20, "thoughtsTokenCount": 5, "totalTokenCount": 155}}),
        ]);
        let (mut model, task) = server(vec![(200, first), (200, second)]).await;
        model.id = id.into();
        model.max_tokens = 1_048_576;
        // Each hop's prompt stays below this tier, while their summed billing
        // input exceeds it. Cost must be calculated per hop before aggregation.
        model.cost.tiers = Some(vec![CostTier {
            context_over: 200,
            input: 4.0,
            output: 20.0,
            cached_read: 0.2,
            cached_write: 0.0,
        }]);
        let received = events(&model, context(), long_decode_options()).await;
        let message = terminal(&received);
        assert_eq!(message.stop_reason, StopReason::ToolUse);
        assert_eq!(
            (
                message.usage.input,
                message.usage.cache_read,
                message.usage.output
            ),
            (90, 140, 55)
        );
        assert_eq!(message.usage.reasoning, Some(35));
        assert_eq!(message.usage.total_tokens, 285);
        assert_eq!(message.usage.context_tokens, Some(155));
        assert!((message.usage.cost.total - 0.000_744).abs() < 1e-12);
        assert_eq!(
            received
                .iter()
                .filter(|event| matches!(event, AssistantMessageEvent::Start))
                .count(),
            1
        );
        assert_eq!(text_deltas(&received), "Reading.");
        let thoughts: String = received
            .iter()
            .filter_map(|event| match event {
                AssistantMessageEvent::ThinkingDelta { delta, .. } => Some(delta.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(thoughts, "Plan carefully.");
        let calls: Vec<&ToolCall> = message
            .content
            .iter()
            .filter_map(|content| match content {
                AssistantContent::ToolCall(call) => Some(call),
                _ => None,
            })
            .collect();
        assert_eq!(calls.len(), 1);
        assert_eq!(
            calls[0].thought_signature.as_deref(),
            Some("retained-signature")
        );
        let mut requests = completed_requests(task).await;
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[0].body["generationConfig"]["maxOutputTokens"],
            200_000
        );
        assert_eq!(
            requests[1]
                .body
                .as_object_mut()
                .unwrap()
                .remove("continuationToken"),
            Some(json!("checkpoint-1"))
        );
        assert_eq!(
            requests[1].body, requests[0].body,
            "continuation must replay the original request unchanged"
        );
    }
}

#[tokio::test]
async fn long_decode_rejects_missing_empty_and_reused_checkpoint_tokens() {
    for token in [None, Some(""), Some("reused")] {
        let mut replies = Vec::new();
        if token == Some("reused") {
            replies.push((
                200,
                sse(&[
                    json!({"candidates": [{"content": {"parts": [{"text": "committed"}]},
                "finishReason": "CONTINUATION", "continuationToken": "reused"}],
                "usageMetadata": {"promptTokenCount": 100, "candidatesTokenCount": 10, "totalTokenCount": 110}}),
                ]),
            ));
        }
        let mut candidate = json!({"content": {"parts": [{"text": "invalid checkpoint"}]}, "finishReason": "CONTINUATION"});
        if let Some(token) = token {
            candidate["continuationToken"] = json!(token);
        }
        replies.push((200, sse(&[json!({"candidates": [candidate]})])));
        let expected_requests = replies.len();
        let (mut model, task) = server(replies).await;
        model.max_tokens = 1_048_576;
        let received = events(&model, context(), long_decode_options()).await;
        let message = terminal(&received);
        assert_eq!(message.stop_reason, StopReason::Error, "token {token:?}");
        assert_eq!(
            message.error_kind,
            Some(ErrorKind::NonReplayable),
            "token {token:?}"
        );
        assert!(!crate::retry::is_retryable_assistant_error(message));
        assert_eq!(completed_requests(task).await.len(), expected_requests);
    }
}

#[tokio::test]
async fn max_tokens_finish_never_resumes_even_with_a_continuation_token() {
    let (mut model, task) = server(vec![(200, sse(&[json!({"candidates": [{"content": {"parts": [{"text": "budget exhausted"}]},
        "finishReason": "MAX_TOKENS", "continuationToken": "do-not-resume"}],
        "usageMetadata": {"promptTokenCount": 100, "candidatesTokenCount": 200_000, "totalTokenCount": 200_100}})]))]).await;
    model.max_tokens = 1_048_576;
    let received = events(&model, context(), long_decode_options()).await;
    assert_eq!(terminal(&received).stop_reason, StopReason::Length);
    assert_eq!(text_deltas(&received), "budget exhausted");
    let requests = completed_requests(task).await;
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].body["generationConfig"]["maxOutputTokens"],
        200_000
    );
}

#[tokio::test]
async fn interrupted_long_decode_resumes_only_when_checkpoint_covers_emitted_parts() {
    for finish_reason in [None, Some("CONTINUATION")] {
        let uncheckpointed_usage = sse(&[json!({"usageMetadata": {
            "promptTokenCount": 10_000, "cachedContentTokenCount": 5_000,
            "candidatesTokenCount": 1_000, "thoughtsTokenCount": 100, "totalTokenCount": 11_100}})]);
        let mut checkpoint = json!({"candidates": [{"content": {"parts": [{"text": "kept before "}]},
            "continuationToken": "saved-checkpoint"}],
            "usageMetadata": {"promptTokenCount": 100, "cachedContentTokenCount": 20,
                "candidatesTokenCount": 15, "thoughtsTokenCount": 5, "totalTokenCount": 120}});
        if let Some(reason) = finish_reason {
            checkpoint["candidates"][0]["finishReason"] = json!(reason);
            checkpoint["usageMetadata"]["candidatesTokenCount"] = json!(5);
            checkpoint["usageMetadata"]["thoughtsTokenCount"] = json!(0);
            checkpoint["usageMetadata"]["totalTokenCount"] = json!(105);
        }
        let checkpoint = sse(&[checkpoint]);
        // Usage after a terminal checkpoint is its final cumulative total.
        // After an intermediate checkpoint it may include hidden uncommitted work.
        let trailing_usage = if finish_reason.is_some() {
            sse(&[
                json!({"usageMetadata": {"promptTokenCount": 100, "cachedContentTokenCount": 20,
                "candidatesTokenCount": 15, "thoughtsTokenCount": 5, "totalTokenCount": 120}}),
            ])
        } else {
            uncheckpointed_usage.clone()
        };
        let interrupted = format!("{checkpoint}{trailing_usage}");
        let final_hop = sse(&[json!({"candidates": [{"content": {"parts": [
            {"text": "kept after"},
            {"functionCall": {"id": "retained", "name": "read", "args": {"path": "retained.rs"}}, "thoughtSignature": "retained-signature"}
        ]}, "finishReason": "STOP"}], "usageMetadata": {"promptTokenCount": 120, "cachedContentTokenCount": 90,
            "candidatesTokenCount": 25, "thoughtsTokenCount": 10, "totalTokenCount": 155}})]);
        let (mut model, task) = scripted_server(vec![
            (200, uncheckpointed_usage, true),
            (200, interrupted, true),
            (200, final_hop, false),
        ])
        .await;
        model.id = "gemini-4-argon".into();
        model.max_tokens = 1_048_576;
        let mut options = long_decode_options();
        options.max_retries = Some(1);
        let received = events(&model, context(), options).await;
        let message = terminal(&received);
        assert_eq!(
            message.stop_reason,
            StopReason::ToolUse,
            "checkpoint finish reason {finish_reason:?}"
        );
        assert_eq!(text_deltas(&received), "kept before kept after");
        assert_eq!(
            (
                message.usage.input,
                message.usage.cache_read,
                message.usage.output
            ),
            (110, 110, 55),
            "only checkpointed usage and the completed final hop are billed"
        );
        assert_eq!(message.usage.reasoning, Some(15));
        assert_eq!(message.usage.total_tokens, 275);
        assert_eq!(message.usage.context_tokens, Some(155));
        assert_eq!(
            received
                .iter()
                .filter(|event| matches!(event, AssistantMessageEvent::ToolCallEnd { .. }))
                .count(),
            1
        );
        assert_eq!(
            received
                .iter()
                .filter(|event| matches!(event, AssistantMessageEvent::Start))
                .count(),
            1
        );
        let requests = completed_requests(task).await;
        assert_eq!(requests.len(), 3);
        assert_eq!(
            requests[0].body, requests[1].body,
            "before a checkpoint, retry the original request"
        );
        let mut expected_resume = requests[0].body.clone();
        expected_resume["continuationToken"] = json!("saved-checkpoint");
        assert_eq!(
            requests[2].body, expected_resume,
            "retry the latest checkpoint without replaying partial output"
        );
    }
}
#[tokio::test]
async fn interrupted_long_decode_obeys_retry_limits_and_mode_boundaries() {
    for (id, budget, max_retries, attempts) in [
        ("gemini-4-carbon", 200_000, Some(0), 1),
        ("gemini-4-carbon", 200_000, None, 4),
        ("gemini-4-carbon", 65_536, None, 1),
        ("gemini-3.8-flash", 200_000, None, 1),
    ] {
        let body = sse(&[json!({"usageMetadata": {"promptTokenCount": 100}})]);
        let (mut model, finished, task) = repeating_server(body, true).await;
        model.id = id.into();
        let mut options = long_decode_options();
        options.max_tokens = Some(budget);
        options.max_retries = max_retries;
        let received = events(&model, context(), options).await;
        finished.cancel();
        let message = terminal(&received);
        assert_eq!(message.stop_reason, StopReason::Error);
        assert_eq!(message.error_kind, Some(ErrorKind::Transport));
        assert!(message.content.is_empty());
        assert_eq!(
            completed_requests(task).await.len(),
            attempts,
            "{id} at {budget}"
        );
    }
}

#[tokio::test]
async fn emitted_suffixes_are_retained_and_never_replayed_after_interruption() {
    let parts = [
        (json!({"text": "visible-text"}), "visible-text"),
        (
            json!({"thought": true, "text": "visible-thought"}),
            "visible-thought",
        ),
        (
            json!({"text": "", "thoughtSignature": "visible-empty-signature"}),
            "visible-empty-signature",
        ),
        (
            json!({"functionCall": {"id": "visible-call", "name": "read", "args": {"path": "a.rs"}}}),
            "visible-call",
        ),
    ];
    for (checkpointed, interrupted, repeat_token) in [
        (false, true, false),
        (true, true, false),
        (true, true, true),
        (true, false, true),
    ] {
        for (part, marker) in &parts {
            let mut chunks = Vec::new();
            if checkpointed {
                chunks.push(json!({"candidates": [{"content": {"parts": [{"text": "checkpointed "}]},
                    "continuationToken": "checkpoint"}],
                    "usageMetadata": {"promptTokenCount": 100, "candidatesTokenCount": 10, "totalTokenCount": 110}}));
            }
            chunks.push(json!({"candidates": [{"content": {"parts": [part]}}],
                "usageMetadata": {"promptTokenCount": 100, "candidatesTokenCount": 50, "totalTokenCount": 150}}));
            if repeat_token {
                let mut candidate = json!({"continuationToken": "checkpoint"});
                if !interrupted {
                    candidate["finishReason"] = json!("CONTINUATION");
                }
                chunks.push(json!({"candidates": [candidate]}));
            }
            let (model, finished, task) = repeating_server(sse(&chunks), interrupted).await;
            let received = events(&model, context(), long_decode_options()).await;
            finished.cancel();
            let message = terminal(&received);
            assert_eq!(message.stop_reason, StopReason::Error, "{marker}");
            assert_eq!(
                message.error_kind,
                Some(ErrorKind::NonReplayable),
                "{marker}"
            );
            assert!(
                !crate::retry::is_retryable_assistant_error(message),
                "{marker}"
            );
            assert!(
                serde_json::to_string(&message.content)
                    .unwrap()
                    .contains(marker),
                "partial content was lost: {marker}"
            );
            if interrupted {
                assert_eq!(
                    message.usage.output,
                    if checkpointed { 10 } else { 0 },
                    "repeated tokens must not advance checkpoint billing"
                );
            }
            assert_eq!(
                completed_requests(task).await.len(),
                1,
                "unsafe suffix must not trigger another request: {marker}"
            );
        }
    }
}

#[tokio::test]
async fn cancelled_long_decode_keeps_emitted_suffix_and_bills_only_checkpoint() {
    use futures_util::StreamExt as _;
    use tokio::io::AsyncWriteExt as _;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut model = fixture_model();
    model.base_url = format!("http://{}/v1beta", listener.local_addr().unwrap());
    model.max_tokens = 1_048_576;
    let signal = CancellationToken::new();
    let stop = signal.clone();
    let task = tokio::spawn(async move {
        let checkpoint = sse(&[
            json!({"candidates": [{"content": {"parts": [{"text": "checkpointed "}]},
            "finishReason": "CONTINUATION", "continuationToken": "saved-checkpoint"}],
            "usageMetadata": {"promptTokenCount": 100, "candidatesTokenCount": 10, "totalTokenCount": 110}}),
        ]);
        let (socket, _) = listener.accept().await.unwrap();
        let (_, mut writer) = capture_request(socket).await;
        writer.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{checkpoint}", checkpoint.len()).as_bytes()).await.unwrap();
        writer.shutdown().await.unwrap();
        let (socket, _) = listener.accept().await.unwrap();
        let (_, mut writer) = capture_request(socket).await;
        writer
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
        writer.write_all(sse(&[json!({"candidates": [{"content": {"parts": [{"text": "live suffix"}]}}],
            "usageMetadata": {"promptTokenCount": 110, "candidatesTokenCount": 50, "totalTokenCount": 160}})]).as_bytes()).await.unwrap();
        stop.cancelled().await;
        assert!(
            tokio::time::timeout(Duration::from_millis(200), listener.accept())
                .await
                .is_err(),
            "cancellation must not open another request"
        );
    });
    let mut options = long_decode_options();
    options.signal = Some(signal.clone());
    let mut stream = GoogleProvider::new().stream(&model, context(), options);
    let received = tokio::time::timeout(Duration::from_secs(3), async {
        let mut received = Vec::new();
        while let Some(event) = stream.next().await {
            if matches!(&event, AssistantMessageEvent::TextDelta { delta, .. } if delta == "live suffix") {
                signal.cancel();
            }
            received.push(event);
        }
        received
    }).await.expect("cancel after the suffix is actually emitted");
    let message = terminal(&received);
    assert_eq!(message.stop_reason, StopReason::Aborted);
    assert_eq!(message.error_kind, Some(ErrorKind::Aborted));
    assert_eq!(text_deltas(&received), "checkpointed live suffix");
    assert!(
        serde_json::to_string(&message.content)
            .unwrap()
            .contains("live suffix")
    );
    assert_eq!(
        (
            message.usage.input,
            message.usage.output,
            message.usage.total_tokens
        ),
        (100, 10, 110)
    );
    task.await.unwrap();
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
    for id in ["gemini-4-argon", "gemini-4-carbon", "gemini-4-hydrogen"] {
        let mut model = crate::catalog::builtin_models()
            .into_iter()
            .find(|model| model.id == id)
            .expect("native Gemini 4 model");
        model.max_tokens = 16_384;
        let highest = if id == "gemini-4-hydrogen" {
            "XHIGH"
        } else {
            "HIGH"
        };
        for (reasoning, expected) in [
            (None, "LOW"),
            (Some(ThinkingLevel::Minimal), "LOW"),
            (Some(ThinkingLevel::XHigh), highest),
            (Some(ThinkingLevel::Max), highest),
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
        model.max_tokens = 1_048_576;
        let mut anchored = context.clone();
        let mut previous = crate::providers::new_output_message(&model);
        previous.stop_reason = StopReason::Stop;
        previous.usage.total_tokens = 2_000_000;
        previous.usage.context_tokens = Some(900_000);
        anchored.messages.push(Message::Assistant(previous));
        for (requested, expected) in [(u64::MAX, 144_480), (1_000, 1_000)] {
            let mut options = options();
            options.max_tokens = Some(requested);
            let body = build_request_body(&model, &anchored, &options, &GoogleCompat::default());
            assert_eq!(
                body["generationConfig"]["maxOutputTokens"], expected,
                "{id}: cap against retained context and safety headroom, preserving smaller budgets"
            );
        }
    }
}
