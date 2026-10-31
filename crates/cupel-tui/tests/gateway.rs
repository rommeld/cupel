//! Run the real binary against a local gateway in Anthropic and Bedrock formats.

#[cfg(test)]
mod tests {
    use std::io::{BufRead as _, BufReader, Read as _, Write as _};
    use std::process::{Command, Stdio};

    use serde_json::{Value, json};

    const REPLY: &str = "hello from the fake gateway";

    fn fake_gateway() -> (String, std::sync::mpsc::Receiver<String>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (heads, received) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for socket in listener.incoming() {
                let mut socket = socket.unwrap();
                let mut reader = BufReader::new(socket.try_clone().unwrap());
                let mut head = String::new();
                let mut content_length = 0;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" || line.is_empty() {
                        break;
                    }
                    if let Some((name, value)) = line.split_once(':')
                        && name.eq_ignore_ascii_case("content-length")
                    {
                        content_length = value.trim().parse().unwrap();
                    }
                    head.push_str(&line);
                }
                reader.read_exact(&mut vec![0; content_length]).unwrap();
                let (content_type, body) = if head.contains("/converse-stream ") {
                    ("application/vnd.amazon.eventstream", converse_stream())
                } else {
                    ("text/event-stream", messages_stream())
                };
                heads.send(head).unwrap();
                write!(
                    socket,
                    "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .unwrap();
                socket.write_all(&body).unwrap();
            }
        });
        (url, received)
    }

    fn messages_stream() -> Vec<u8> {
        [
            json!({"type": "message_start", "message": {"id": "msg_gateway", "usage": {"input_tokens": 10, "output_tokens": 1}}}),
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": REPLY}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 5}}),
            json!({"type": "message_stop"}),
        ]
        .iter()
        .flat_map(|event| format!("data: {event}\n\n").into_bytes())
        .collect()
    }

    fn converse_stream() -> Vec<u8> {
        [
            frame("messageStart", &json!({"role": "assistant"})),
            frame(
                "contentBlockDelta",
                &json!({"contentBlockIndex": 0, "delta": {"text": REPLY}}),
            ),
            frame("contentBlockStop", &json!({"contentBlockIndex": 0})),
            frame("messageStop", &json!({"stopReason": "end_turn"})),
            frame(
                "metadata",
                &json!({"usage": {"inputTokens": 10, "outputTokens": 5, "totalTokens": 15}, "metrics": {"latencyMs": 1}}),
            ),
        ]
        .concat()
    }

    // AWS event stream lengths and checksums are big endian on the wire.
    #[allow(clippy::big_endian_bytes)]
    fn frame(event_type: &str, payload: &Value) -> Vec<u8> {
        let mut headers = Vec::new();
        for (name, value) in [
            (":event-type", event_type),
            (":content-type", "application/json"),
            (":message-type", "event"),
        ] {
            headers.push(u8::try_from(name.len()).unwrap());
            headers.extend_from_slice(name.as_bytes());
            // Value type 7 is a string with a 2 byte length.
            headers.push(7);
            headers.extend_from_slice(&u16::try_from(value.len()).unwrap().to_be_bytes());
            headers.extend_from_slice(value.as_bytes());
        }
        let payload = payload.to_string().into_bytes();
        // 12 bytes of prelude (two lengths and their CRC), 4 of trailing CRC.
        let total = 12 + headers.len() + payload.len() + 4;
        let mut frame = Vec::new();
        frame.extend_from_slice(&u32::try_from(total).unwrap().to_be_bytes());
        frame.extend_from_slice(&u32::try_from(headers.len()).unwrap().to_be_bytes());
        frame.extend_from_slice(&crc32(&frame).to_be_bytes());
        frame.extend_from_slice(&headers);
        frame.extend_from_slice(&payload);
        frame.extend_from_slice(&crc32(&frame).to_be_bytes());
        frame
    }

    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = u32::MAX;
        for byte in bytes {
            crc ^= u32::from(*byte);
            for _ in 0..8 {
                crc = if crc & 1 == 1 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }

    fn run_cupel(name: &str, vars: &[(&str, &str)]) -> String {
        let root = std::env::temp_dir().join(format!(
            "cupel-gateway-{name}-{}-{}",
            std::process::id(),
            cupel_core::types::now_ms()
        ));
        let project = root.join("project");
        std::fs::create_dir_all(&project).unwrap();

        let mut child = Command::new(env!("CARGO_BIN_EXE_cupel"))
            .arg("--plain")
            .current_dir(&project)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            // HOME also hides ~/.aws from the SDK's credential chain.
            .env("HOME", &root)
            .env("CUPEL_HOME", root.join("home"))
            .env("OLLAMA_HOST", "http://127.0.0.1:9")
            .env("AWS_EC2_METADATA_DISABLED", "true")
            .envs(vars.iter().copied())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(b"say hi\n").unwrap();
        let output = child.wait_with_output().unwrap();
        std::fs::remove_dir_all(root).unwrap();

        let stdout = String::from_utf8(output.stdout).unwrap();
        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(output.status.success(), "{stdout}\n{stderr}");
        stdout
    }

    #[test]
    fn anthropic_format_sends_the_auth_token_as_a_bearer_token() {
        let (gateway, heads) = fake_gateway();
        let stdout = run_cupel(
            "anthropic",
            &[
                ("ANTHROPIC_BASE_URL", gateway.as_str()),
                ("ANTHROPIC_AUTH_TOKEN", "gateway-key"),
                ("ANTHROPIC_API_KEY", "not-this-key"),
            ],
        );
        assert!(stdout.contains(REPLY), "{stdout}");

        let head = heads.try_recv().unwrap();
        assert!(head.starts_with("POST /v1/messages "), "{head}");
        assert!(
            head.contains("\r\nauthorization: Bearer gateway-key\r\n"),
            "{head}"
        );
        assert!(!head.contains("x-api-key"), "{head}");
    }

    #[test]
    fn bedrock_format_needs_only_the_bearer_token_and_the_endpoint() {
        let (gateway, heads) = fake_gateway();
        let endpoint = format!("{gateway}/bedrock");
        let stdout = run_cupel(
            "bedrock",
            &[
                ("AWS_BEARER_TOKEN_BEDROCK", "gateway-key"),
                ("AWS_ENDPOINT_URL_BEDROCK_RUNTIME", endpoint.as_str()),
                ("AWS_REGION", "us-east-1"),
            ],
        );
        assert!(stdout.contains("(amazon-bedrock)"), "{stdout}");
        assert!(stdout.contains(REPLY), "{stdout}");

        let head = heads.try_recv().unwrap();
        assert!(head.starts_with("POST /bedrock/model/"), "{head}");
        assert!(head.contains("/converse-stream HTTP/1.1\r\n"), "{head}");
        assert!(
            head.contains("\r\nauthorization: Bearer gateway-key\r\n"),
            "{head}"
        );
    }
}
