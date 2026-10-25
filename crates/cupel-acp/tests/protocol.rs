//! Real stdio process against deterministic loopback inference. No credentials.

#[cfg(test)]
mod tests {

    use std::path::PathBuf;
    use std::process::Stdio;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    use serde_json::{Value, json};
    use tokio::io::{
        AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader, Lines,
    };
    use tokio::net::{TcpListener, TcpStream};
    use tokio::process::{Child, ChildStdin, ChildStdout, Command};
    use tokio::task::JoinHandle;

    struct Fixture {
        root: PathBuf,
        server: JoinHandle<()>,
    }

    impl Fixture {
        async fn new() -> Self {
            let root = std::env::temp_dir().join(format!("cupel-acp-{}", random_id()));
            std::fs::create_dir_all(root.join("home")).unwrap();
            std::fs::create_dir_all(root.join("project")).unwrap();
            std::fs::write(root.join("project/marker.txt"), "project-specific-content").unwrap();
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}/v1", listener.local_addr().unwrap());
            std::fs::write(
            root.join("home/models.json"),
            json!([{
                "id":"fixture", "name":"Fixture", "api":"openai-completions", "provider":"fixture",
                "baseUrl":url, "reasoning":false, "input":["text"],
                "cost":{"input":0,"output":0,"cachedRead":0,"cachedWrite":0},
                "contextWindow":32768,"maxTokens":1024,"compat":{"requiresApiKey":false}
            }])
            .to_string(),
        )
        .unwrap();
            let server = tokio::spawn(async move {
                let calls = std::sync::Arc::new(AtomicU64::new(0));
                while let Ok((stream, _)) = listener.accept().await {
                    let call = calls.fetch_add(1, Ordering::Relaxed);
                    tokio::spawn(async move { serve(stream, call).await });
                }
            });
            Self { root, server }
        }

        fn start(&self) -> Client {
            let mut process = Command::new(env!("CARGO_BIN_EXE_cupel-acp"))
                .args(["--model", "fixture"])
                .current_dir(&self.root)
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("HOME", self.root.join("home"))
                .env("CUPEL_HOME", self.root.join("home"))
                .env("OLLAMA_HOST", "http://127.0.0.1:9")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .kill_on_drop(true)
                .spawn()
                .unwrap();
            let input = process.stdin.take();
            let output = BufReader::new(process.stdout.take().unwrap()).lines();
            Client {
                process,
                input,
                output,
            }
        }

        fn cwd(&self) -> PathBuf {
            self.root.join("project").canonicalize().unwrap()
        }

        fn transcript(&self, id: &str) -> PathBuf {
            cupel_coding_agent::session::sessions_dir(Some(&self.root.join("home")), &self.cwd())
                .unwrap()
                .join(format!("{id}.jsonl"))
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            self.server.abort();
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn random_id() -> String {
        let mut bytes = [0_u8; 8];
        getrandom::fill(&mut bytes).unwrap();
        bytes
            .iter()
            .map(u8::to_string)
            .collect::<Vec<_>>()
            .join("-")
    }

    struct Client {
        process: Child,
        input: Option<ChildStdin>,
        output: Lines<BufReader<ChildStdout>>,
    }

    impl Client {
        async fn send(&mut self, value: Value) {
            self.input
                .as_mut()
                .unwrap()
                .write_all(format!("{value}\n").as_bytes())
                .await
                .unwrap();
        }

        async fn next(&mut self) -> Value {
            let line = tokio::time::timeout(Duration::from_secs(10), self.output.next_line())
                .await
                .expect("ACP response timeout")
                .unwrap()
                .expect("unexpected ACP EOF");
            serde_json::from_str(&line).expect("stdout must contain only ACP JSON")
        }

        async fn until(&mut self, id: u64) -> Vec<Value> {
            let mut messages = Vec::new();
            loop {
                let message = self.next().await;
                let done = message["id"] == id;
                messages.push(message);
                if done {
                    return messages;
                }
            }
        }

        async fn request(&mut self, id: u64, method: &str, params: Value) -> Vec<Value> {
            self.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
                .await;
            self.until(id).await
        }

        async fn init(&mut self) {
            let result = self
                .request(
                    1,
                    "initialize",
                    json!({"protocolVersion":1,"clientCapabilities":{}}),
                )
                .await;
            assert_eq!(
                result.last().unwrap()["result"]["protocolVersion"],
                1,
                "must select ACP v1"
            );
            assert_eq!(
                result.last().unwrap()["result"]["agentCapabilities"]["loadSession"],
                true,
                "persistent sessions must advertise load support"
            );
        }

        async fn new_session(&mut self, cwd: &std::path::Path) -> String {
            let result = self
                .request(2, "session/new", json!({"cwd":cwd,"mcpServers":[]}))
                .await;
            result.last().unwrap()["result"]["sessionId"]
                .as_str()
                .unwrap_or_else(|| panic!("{result:?}"))
                .to_owned()
        }

        async fn prompt(&mut self, session: &str, id: u64, text: &str) -> Vec<Value> {
            self.request(id, "session/prompt", params(session, text))
                .await
        }

        async fn close(&mut self) {
            self.input.take();
            let status = tokio::time::timeout(Duration::from_secs(10), self.process.wait())
                .await
                .unwrap()
                .unwrap();
            assert!(status.success(), "{status}");
        }
    }

    fn params(session: &str, text: &str) -> Value {
        json!({"sessionId":session,"prompt":[{"type":"text","text":text}]})
    }

    fn text(messages: &[Value]) -> String {
        messages
            .iter()
            .filter(|m| m["params"]["update"]["sessionUpdate"] == "agent_message_chunk")
            .filter_map(|m| m["params"]["update"]["content"]["text"].as_str())
            .collect()
    }

    async fn serve(stream: TcpStream, call: u64) {
        let (read, mut write) = stream.into_split();
        let mut read = BufReader::new(read);
        let mut length = 0;
        loop {
            let mut line = String::new();
            if read.read_line(&mut line).await.unwrap() == 0 {
                return;
            }
            if line == "\r\n" {
                break;
            }
            if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                length = value.trim().parse().unwrap();
            }
        }
        let mut body = vec![0; length];
        read.read_exact(&mut body).await.unwrap();
        let body: Value = serde_json::from_slice(&body).unwrap();
        let messages = body["messages"].as_array().unwrap();
        let users: Vec<_> = messages.iter().filter(|m| m["role"] == "user").collect();
        let task = users.last().unwrap()["content"].as_str().unwrap();
        if task == "fail" {
            let body = r#"{"error":{"message":"fixture failure"}}"#;
            let _ = write.write_all(format!("HTTP/1.1 400 Bad Request\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await;
            return;
        }
        let _ = write
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .await;
        let final_tool = messages.last().unwrap()["role"] == "tool";
        let (delta, reason) = if matches!(task, "read" | "bad-read" | "guard" | "sleep")
            && !final_tool
        {
            let (name, args) = match task {
                "read" => ("read", json!({"path":"marker.txt"})),
                "bad-read" => ("read", json!({"path":"absent.txt"})),
                "guard" => ("bash", json!({"command":"git push --force"})),
                _ => (
                    "bash",
                    json!({"command":"sleep 30 & child=$!; echo $child > child.pid; wait"}),
                ),
            };
            (
                json!({"role":"assistant","tool_calls":[{"index":0,"id":format!("call_{call}"),"type":"function","function":{"name":name,"arguments":args.to_string()}}]}),
                "tool_calls",
            )
        } else {
            let answer = if final_tool {
                messages.last().unwrap()["content"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            } else {
                format!("reply:{task};users={}", users.len())
            };
            (
                json!({"role":"assistant","content":answer}),
                if task == "length" { "length" } else { "stop" },
            )
        };
        let chunk = json!({"id":format!("r{call}"),"object":"chat.completion.chunk","choices":[{"index":0,"delta":delta,"finish_reason":null}]});
        if write
            .write_all(format!("data: {chunk}\n\n").as_bytes())
            .await
            .is_err()
        {
            return;
        }
        if task == "slow" {
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        let done = json!({"id":format!("r{call}"),"object":"chat.completion.chunk","choices":[{"index":0,"delta":{},"finish_reason":reason}]});
        let _ = write
            .write_all(format!("data: {done}\n\ndata: [DONE]\n\n").as_bytes())
            .await;
    }

    #[tokio::test]
    async fn streams_tools_preserves_turns_and_replays_without_duplicate_recording() {
        let fixture = Fixture::new().await;
        let mut client = fixture.start();
        client.init().await;
        let id = client.new_session(&fixture.cwd()).await;
        let first = client.prompt(&id, 3, "read").await;
        assert!(
            text(&first).contains("project-specific-content"),
            "{first:?}"
        );
        let statuses: Vec<_> = first
            .iter()
            .filter_map(|m| m["params"]["update"]["status"].as_str())
            .collect();
        assert!(statuses.contains(&"in_progress"));
        assert!(statuses.contains(&"completed"));
        let second = client.prompt(&id, 4, "second").await;
        assert_eq!(text(&second), "reply:second;users=2");
        client.close().await;
        let before = std::fs::read(fixture.transcript(&id)).unwrap();
        let mut resumed = fixture.start();
        resumed.init().await;
        let replay = resumed
            .request(
                2,
                "session/load",
                json!({"sessionId":id,"cwd":fixture.cwd(),"mcpServers":[]}),
            )
            .await;
        assert!(replay.last().unwrap().get("result").is_some(), "{replay:?}");
        assert!(text(&replay).contains("reply:second;users=2"));
        assert_eq!(std::fs::read(fixture.transcript(&id)).unwrap(), before);
        assert_eq!(
            text(&resumed.prompt(&id, 5, "third").await),
            "reply:third;users=3"
        );
        resumed.close().await;
    }

    #[tokio::test]
    async fn rejects_invalid_requests_without_losing_the_connection() {
        let fixture = Fixture::new().await;
        let mut client = fixture.start();
        let missing_init = client
            .request(
                0,
                "session/new",
                json!({"cwd":fixture.cwd(),"mcpServers":[]}),
            )
            .await;
        assert!(missing_init.last().unwrap().get("error").is_some());
        client.init().await;
        for (id, method, params) in [
            (3, "session/new", json!({"cwd":"relative","mcpServers":[]})),
            (
                4,
                "session/new",
                json!({"cwd":fixture.cwd(),"mcpServers":[{"name":"x","command":"false","args":[],"env":[]}]}),
            ),
            (
                5,
                "session/load",
                json!({"sessionId":"../escape","cwd":fixture.cwd(),"mcpServers":[]}),
            ),
            (6, "missing/method", json!({})),
        ] {
            assert!(
                client
                    .request(id, method, params)
                    .await
                    .last()
                    .unwrap()
                    .get("error")
                    .is_some()
            );
        }
        let id = client.new_session(&fixture.cwd()).await;
        assert!(
            client
                .prompt(&id, 7, "")
                .await
                .last()
                .unwrap()
                .get("error")
                .is_some()
        );
        let image = client.request(8,"session/prompt",json!({"sessionId":id,"prompt":[{"type":"image","data":"AA==","mimeType":"image/png"}]})).await;
        assert!(image.last().unwrap().get("error").is_some());
        assert_eq!(text(&client.prompt(&id, 9, "ok").await), "reply:ok;users=1");
        client.close().await;
    }

    #[tokio::test]
    async fn cancellation_and_busy_admission_are_ordered_before_background_work() {
        let fixture = Fixture::new().await;
        let mut client = fixture.start();
        client.init().await;
        let id = client.new_session(&fixture.cwd()).await;
        client
        .send(json!({"jsonrpc":"2.0","id":3,"method":"session/prompt","params":params(&id,"slow")}))
        .await;
        // Wait for a streamed token, then overlap and cancel while inference is pending.
        let first = client.next().await;
        assert_eq!(first["method"], "session/update");
        let busy = client.prompt(&id, 4, "overlap").await;
        assert!(busy.last().unwrap().get("error").is_some());
        client
            .send(json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":id}}))
            .await;
        assert_eq!(
            client.until(3).await.last().unwrap()["result"]["stopReason"],
            "cancelled"
        );
        // Same packet: cancel must see the prompt before its task is polled.
        let packet = format!(
            "{}\n{}\n",
            json!({"jsonrpc":"2.0","id":5,"method":"session/prompt","params":params(&id,"slow")}),
            json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":id}})
        );
        client
            .input
            .as_mut()
            .unwrap()
            .write_all(packet.as_bytes())
            .await
            .unwrap();
        assert_eq!(
            client.until(5).await.last().unwrap()["result"]["stopReason"],
            "cancelled"
        );
        assert_eq!(
            client.prompt(&id, 6, "after").await.last().unwrap()["result"]["stopReason"],
            "end_turn"
        );
        client.close().await;
    }

    #[tokio::test]
    async fn failures_length_and_guard_denials_have_distinct_outcomes() {
        let fixture = Fixture::new().await;
        let mut client = fixture.start();
        client.init().await;
        let id = client.new_session(&fixture.cwd()).await;
        for (n, task) in [(3, "bad-read"), (4, "guard")] {
            let result = client.prompt(&id, n, task).await;
            assert!(
                result
                    .iter()
                    .any(|m| m["params"]["update"]["status"] == "failed"),
                "{result:?}"
            );
        }
        let result = client.prompt(&id, 5, "fail").await;
        assert_eq!(result.last().unwrap()["error"]["code"], -32603);
        assert_eq!(
            client.prompt(&id, 6, "length").await.last().unwrap()["result"]["stopReason"],
            "max_tokens"
        );
        client.close().await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn eof_cleans_up_running_bash_descendants() {
        let fixture = Fixture::new().await;
        let mut client = fixture.start();
        client.init().await;
        let id = client.new_session(&fixture.cwd()).await;
        client
        .send(
            json!({"jsonrpc":"2.0","id":3,"method":"session/prompt","params":params(&id,"sleep")}),
        )
        .await;
        let pid_file = fixture.cwd().join("child.pid");
        tokio::time::timeout(Duration::from_secs(10), async {
            while !pid_file.exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        let pid = std::fs::read_to_string(pid_file).unwrap();
        client.close().await;
        let status = Command::new("/bin/kill")
            .args(["-0", pid.trim()])
            .stderr(Stdio::null())
            .status()
            .await
            .unwrap();
        assert!(!status.success(), "tool descendant {pid} survived ACP EOF");
    }

    #[tokio::test]
    async fn sessions_keep_their_cwd_and_reject_concurrent_writers() {
        let fixture = Fixture::new().await;
        let other = fixture.root.join("other");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("marker.txt"), "other-directory").unwrap();
        let mut client = fixture.start();
        client.init().await;
        let a = client.new_session(&fixture.cwd()).await;
        let b = client.new_session(&fixture.cwd()).await;
        let c = client.new_session(&other).await;
        assert!(text(&client.prompt(&c, 3, "read").await).contains("other-directory"));
        assert!(text(&client.prompt(&a, 4, "read").await).contains("project-specific-content"));
        client
        .send(json!({"jsonrpc":"2.0","id":5,"method":"session/prompt","params":params(&a,"slow")}))
        .await;
        assert_eq!(client.next().await["method"], "session/update");
        let blocked = client.prompt(&b, 6, "second writer").await;
        assert!(
            blocked.last().unwrap()["error"]["message"]
                .as_str()
                .unwrap()
                .contains("directory")
        );
        client
            .send(json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":a}}))
            .await;
        client.until(5).await;
        assert_eq!(
            text(&client.prompt(&b, 7, "after").await),
            "reply:after;users=1"
        );
        client.close().await;
    }

    #[tokio::test]
    async fn load_rejects_a_transcript_whose_header_names_another_directory() {
        let fixture = Fixture::new().await;
        let mut client = fixture.start();
        client.init().await;
        let id = client.new_session(&fixture.cwd()).await;
        client.prompt(&id, 3, "saved").await;
        client.close().await;
        let path = fixture.transcript(&id);
        let data = std::fs::read_to_string(&path).unwrap();
        let mut lines = data.lines();
        let mut header: Value = serde_json::from_str(lines.next().unwrap()).unwrap();
        header["cwd"] = json!(fixture.root.canonicalize().unwrap());
        std::fs::write(
            &path,
            format!("{header}\n{}\n", lines.collect::<Vec<_>>().join("\n")),
        )
        .unwrap();
        let mut client = fixture.start();
        client.init().await;
        let result = client
            .request(
                2,
                "session/load",
                json!({"sessionId":id,"cwd":fixture.cwd(),"mcpServers":[]}),
            )
            .await;
        assert!(
            result.last().unwrap()["error"]["message"]
                .as_str()
                .unwrap()
                .contains("does not match")
        );
        client.close().await;
    }

    #[cfg(unix)]
    fn hook(path: &std::path::Path, script: &str) {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, format!("#!/bin/sh\n{script}\n")).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn project_hooks_require_trust_and_cancellation_during_preparation_stays_responsive() {
        let fixture = Fixture::new().await;
        hook(
            &fixture.cwd().join(".cupel/hooks/user-prompt-submit/probe"),
            "touch project-hook",
        );
        let mut client = fixture.start();
        client.init().await;
        let id = client.new_session(&fixture.cwd()).await;
        client.prompt(&id, 3, "restricted").await;
        assert!(!fixture.cwd().join("project-hook").exists());
        client.close().await;
        cupel_coding_agent::project_trust::save(
            &fixture.root.join("home"),
            &fixture.cwd(),
            cupel_coding_agent::project_trust::ProjectTrust::Trusted,
        )
        .unwrap();
        hook(
            &fixture.cwd().join(".cupel/hooks/user-prompt-submit/probe"),
            "touch preparing; sleep 0.3; touch project-hook",
        );
        let mut client = fixture.start();
        client.init().await;
        let id = client.new_session(&fixture.cwd()).await;
        client.send(json!({"jsonrpc":"2.0","id":3,"method":"session/prompt","params":params(&id,"must not infer")})).await;
        tokio::time::timeout(Duration::from_secs(5), async {
            while !fixture.cwd().join("preparing").exists() {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        client
            .send(json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":id}}))
            .await;
        let result = client.until(3).await;
        assert_eq!(result.last().unwrap()["result"]["stopReason"], "cancelled");
        assert!(text(&result).is_empty());
        assert!(fixture.cwd().join("project-hook").exists());
        client.close().await;
    }
}
