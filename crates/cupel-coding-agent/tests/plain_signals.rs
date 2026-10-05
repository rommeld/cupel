#[cfg(test)]
#[cfg(unix)]
mod tests {
    use std::io::Write as _;
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::Path;
    use std::process::{Command, Stdio};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use cupel_agent::{Agent, AgentMessage, AgentOptions, types::AgentTool};
    use cupel_coding_agent::modes::{SessionMeta, plain};
    use cupel_coding_agent::session::{SessionRecorder, load_transcript};
    use cupel_coding_agent::settings::Settings;
    use cupel_coding_agent::tools::bash::BashTool;
    use cupel_core::event_stream::{AssistantMessageStream, assistant_message_channel};
    use cupel_core::provider::{Provider, Registry};
    use cupel_core::types::{
        Api, AssistantContent, AssistantMessage, Context, InputModality, Message, Model, ModelCost,
        StopReason, StreamOptions, TextContent, ToolCall, Usage, now_ms,
    };

    enum Shutdown {
        Signal(&'static str, i32),
        PipeTools,
        PipeText,
        PipeStartup,
    }

    struct MockProvider {
        pipe_mode: String,
    }

    async fn wait_for_release(root: &Path) {
        while !root.join("release").exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    impl Provider for MockProvider {
        fn api(&self) -> &str {
            "mock"
        }

        fn stream(
            &self,
            model: &Model,
            _context: Context,
            _options: StreamOptions,
        ) -> AssistantMessageStream {
            let (stream, sink) = assistant_message_channel();
            let _ = sink.start();
            let mut message = AssistantMessage {
                content: vec![AssistantContent::ToolCall(ToolCall {
                    id: "sleep".into(),
                    name: "bash".into(),
                    arguments: serde_json::json!({
                        "command": "echo $$ > \"$CUPEL_PID_FILE\"; exec sleep 90"
                    }),
                })],
                api: model.api.clone(),
                provider: model.provider.clone(),
                model: model.id.clone(),
                response_model: None,
                response_id: None,
                usage: Usage::default(),
                stop_reason: StopReason::ToolUse,
                error_message: None,
                timestamp: now_ms(),
            };
            if self.pipe_mode == "tools" {
                // Finish a parallel tool only after the reader closes stdout,
                // forcing EPIPE while the silent sleep is still running.
                message.content.push(AssistantContent::ToolCall(ToolCall {
                    id: "output".into(),
                    name: "bash".into(),
                    arguments: serde_json::json!({
                        "command": "while [ ! -f \"$CUPEL_TEST_ROOT/release\" ]; do sleep 0.02; done; echo tool-finished"
                    }),
                }));
            }
            if self.pipe_mode == "text" {
                message.content = vec![AssistantContent::Text(TextContent::plain("final answer"))];
                message.stop_reason = StopReason::Stop;
                let root = std::env::var("CUPEL_TEST_ROOT").unwrap();
                tokio::spawn(async move {
                    let root = Path::new(&root);
                    std::fs::write(root.join("ready"), "").unwrap();
                    wait_for_release(root).await;
                    let _ = sink.text_delta(0, "final answer".into());
                    let _ = sink.done(StopReason::Stop, message);
                });
            } else {
                let _ = sink.done(StopReason::ToolUse, message);
            }
            stream
        }
    }

    async fn run_child() {
        let root = std::env::var("CUPEL_TEST_ROOT").unwrap();
        let model = Model {
            id: "mock".into(),
            name: "Mock".into(),
            api: Api::from("mock"),
            provider: cupel_core::types::Provider::from("mock"),
            base_url: String::new(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![InputModality::Text],
            cost: ModelCost::default(),
            context_window: 100_000,
            max_context_window: None,
            max_tokens: 4096,
            headers: None,
            compat: None,
        };
        let mut registry = Registry::new();
        let pipe_mode = std::env::var("CUPEL_PIPE_MODE").unwrap_or_default();
        registry.register(Arc::new(MockProvider {
            pipe_mode: pipe_mode.clone(),
        }));
        let mut options = AgentOptions::new(model.clone(), Arc::new(registry));
        options.tools = vec![Arc::new(BashTool::new(&root)) as Arc<dyn AgentTool>];
        let home = Path::new(&root).join("home");
        let recorder = SessionRecorder::new(Some(home), root.as_ref(), "signal-test", "mock");
        let meta = SessionMeta {
            model_name: "Mock".into(),
            provider: "mock".into(),
            cwd: root.clone(),
            templates: Vec::new(),
            models: vec![model],
            home: None,
            settings: Settings::default(),
            startup_warning: None,
            context_files: Vec::new(),
            base_system_prompt: String::new(),
        };
        if pipe_mode == "startup" {
            std::fs::write(Path::new(&root).join("ready"), "").unwrap();
            wait_for_release(Path::new(&root)).await;
        }
        plain::run(Agent::new(options), &meta, recorder)
            .await
            .unwrap();
    }

    fn check_shutdown(shutdown: Shutdown) {
        if std::env::var_os("CUPEL_SIGNAL_CHILD").is_some() {
            tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(run_child());
            // Do not let the test harness print to the closed pipe on return.
            std::process::exit(0);
        }

        let (test_name, pipe_mode, exit_code) = match shutdown {
            Shutdown::Signal("-TERM", code) => (
                "tests::term_to_plain_mode_kills_the_running_bash_group",
                "",
                code,
            ),
            Shutdown::Signal("-HUP", code) => (
                "tests::hup_to_plain_mode_kills_the_running_bash_group",
                "",
                code,
            ),
            Shutdown::Signal("-INT", code) => (
                "tests::int_to_plain_mode_kills_the_running_bash_group",
                "",
                code,
            ),
            Shutdown::Signal(signal, _) => panic!("unexpected signal {signal}"),
            Shutdown::PipeTools => (
                "tests::broken_pipe_kills_the_running_bash_group",
                "tools",
                0,
            ),
            Shutdown::PipeText => ("tests::broken_pipe_preserves_the_final_response", "text", 0),
            Shutdown::PipeStartup => ("tests::broken_pipe_at_startup_exits_quietly", "startup", 0),
        };
        let root = std::env::temp_dir().join(format!(
            "cupel-shutdown-{}-{}-{}",
            std::process::id(),
            test_name,
            now_ms()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let pid_file = root.join("shell.pid");
        let log_path = root.join("output.log");
        let log = std::fs::File::create(&log_path).unwrap();
        let hook_log = root.join("hooks.jsonl");
        for event in ["stop", "session-end"] {
            let dir = root.join("home/hooks").join(event);
            std::fs::create_dir_all(&dir).unwrap();
            let script = dir.join("record");
            std::fs::write(
                &script,
                format!(
                    "#!/bin/sh\n{{ cat; printf '\\n'; }} >> '{}'\n",
                    hook_log.display()
                ),
            )
            .unwrap();
            std::fs::set_permissions(script, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test_name, "--nocapture"])
            .env("CUPEL_SIGNAL_CHILD", "1")
            .env("CUPEL_TEST_ROOT", &root)
            .env("CUPEL_PID_FILE", &pid_file)
            .env("CUPEL_PIPE_MODE", pipe_mode)
            .stdin(Stdio::piped())
            .stdout(if pipe_mode.is_empty() {
                Stdio::from(log.try_clone().unwrap())
            } else {
                Stdio::piped()
            })
            .stderr(log)
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(b"run\n").unwrap();

        let has_bash = matches!(shutdown, Shutdown::Signal(..) | Shutdown::PipeTools);
        let ready = if has_bash {
            pid_file.clone()
        } else {
            root.join("ready")
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        while !ready.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        if !ready.exists() {
            let _ = child.kill();
            panic!(
                "child never became ready: {}",
                std::fs::read_to_string(&log_path).unwrap()
            );
        }
        let pid = has_bash.then(|| {
            std::fs::read_to_string(&pid_file)
                .unwrap()
                .trim()
                .to_string()
        });
        if let Some(pid) = &pid {
            assert!(
                Command::new("kill")
                    .args(["-0", pid])
                    .output()
                    .unwrap()
                    .status
                    .success(),
                "bash child {pid} was not running"
            );
        }
        if let Shutdown::Signal(signal, _) = shutdown {
            assert!(
                Command::new("kill")
                    .args([signal, &child.id().to_string()])
                    .status()
                    .unwrap()
                    .success(),
                "could not send {signal} to cupel"
            );
        } else {
            drop(child.stdout.take());
            std::fs::write(root.join("release"), "").unwrap();
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                if let Some(pid) = &pid {
                    let _ = Command::new("kill").args(["-9", pid]).status();
                }
                let _ = child.kill();
                panic!(
                    "cupel did not exit on shutdown: {}",
                    std::fs::read_to_string(&log_path).unwrap()
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        if let Some(pid) = &pid {
            let survived = Command::new("kill")
                .args(["-0", pid])
                .output()
                .unwrap()
                .status
                .success();
            if survived {
                let _ = Command::new("kill").args(["-9", pid]).status();
            }
            assert!(!survived, "bash child {pid} survived shutdown");
        }
        assert_eq!(
            status.code(),
            Some(exit_code),
            "{}",
            std::fs::read_to_string(&log_path).unwrap()
        );
        if !pipe_mode.is_empty() {
            assert!(
                std::fs::read_to_string(&log_path).unwrap().is_empty(),
                "broken pipe should not report an error"
            );
        }
        if matches!(shutdown, Shutdown::PipeStartup) {
            assert!(!hook_log.exists());
            assert!(!root.join(".cupel").exists());
        } else {
            let hooks: Vec<serde_json::Value> = std::fs::read_to_string(&hook_log)
                .unwrap()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert_eq!(
                hooks
                    .iter()
                    .map(|hook| hook["event"].as_str().unwrap())
                    .collect::<Vec<_>>(),
                ["stop", "session-end"]
            );
            let (_, messages) =
                load_transcript(Path::new(hooks[1]["sessionRef"].as_str().unwrap())).unwrap();
            assert!(matches!(
                messages.first(),
                Some(AgentMessage::Llm(Message::User(_)))
            ));
            if has_bash {
                assert!(
                    messages
                        .iter()
                        .any(|message| matches!(message, AgentMessage::Llm(Message::Assistant(_))))
                );
                assert!(messages.iter().any(|message| matches!(message, AgentMessage::Llm(Message::ToolResult(result)) if result.tool_call_id == "sleep" && result.is_error)));
                if matches!(shutdown, Shutdown::PipeTools) {
                    assert!(messages.iter().any(|message| matches!(message, AgentMessage::Llm(Message::ToolResult(result)) if result.tool_call_id == "output" && !result.is_error)));
                }
            } else {
                assert!(
                    matches!(messages.last(), Some(AgentMessage::Llm(Message::Assistant(message))) if message.content == vec![AssistantContent::Text(TextContent::plain("final answer"))])
                );
            }
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn term_to_plain_mode_kills_the_running_bash_group() {
        check_shutdown(Shutdown::Signal("-TERM", 143));
    }

    #[test]
    fn hup_to_plain_mode_kills_the_running_bash_group() {
        check_shutdown(Shutdown::Signal("-HUP", 129));
    }

    #[test]
    fn int_to_plain_mode_kills_the_running_bash_group() {
        check_shutdown(Shutdown::Signal("-INT", 130));
    }

    #[test]
    fn broken_pipe_kills_the_running_bash_group() {
        check_shutdown(Shutdown::PipeTools);
    }

    #[test]
    fn broken_pipe_preserves_the_final_response() {
        check_shutdown(Shutdown::PipeText);
    }

    #[test]
    fn broken_pipe_at_startup_exits_quietly() {
        check_shutdown(Shutdown::PipeStartup);
    }
}
