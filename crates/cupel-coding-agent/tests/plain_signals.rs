#[cfg(test)]
#[cfg(unix)]
mod tests {
    use std::io::Write as _;
    use std::process::{Command, Stdio};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use cupel_agent::{Agent, AgentOptions, types::AgentTool};
    use cupel_coding_agent::modes::{SessionMeta, plain};
    use cupel_coding_agent::session::SessionRecorder;
    use cupel_coding_agent::settings::Settings;
    use cupel_coding_agent::tools::bash::BashTool;
    use cupel_core::event_stream::{AssistantMessageStream, assistant_message_channel};
    use cupel_core::provider::{Provider, Registry};
    use cupel_core::types::{
        Api, AssistantContent, AssistantMessage, Context, InputModality, Model, ModelCost,
        StopReason, StreamOptions, ToolCall, Usage, now_ms,
    };

    struct LongBash;

    impl Provider for LongBash {
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
            let _ = sink.done(
                StopReason::ToolUse,
                AssistantMessage {
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
                },
            );
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
        registry.register(Arc::new(LongBash));
        let mut options = AgentOptions::new(model.clone(), Arc::new(registry));
        options.tools = vec![Arc::new(BashTool::new(&root)) as Arc<dyn AgentTool>];
        let recorder = SessionRecorder::new(None, root.as_ref(), "signal-test", "mock");
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
        };
        plain::run(Agent::new(options), &meta, recorder)
            .await
            .unwrap();
    }

    fn check_signal(signal: &str, exit_code: i32) {
        if std::env::var_os("CUPEL_SIGNAL_CHILD").is_some() {
            tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(run_child());
            panic!("child exited without receiving a signal");
        }

        let root = std::env::temp_dir().join(format!(
            "cupel-signal-{}-{}-{}",
            std::process::id(),
            signal.trim_start_matches('-'),
            now_ms()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let pid_file = root.join("shell.pid");
        let log_path = root.join("output.log");
        let log = std::fs::File::create(&log_path).unwrap();
        let test_name = match signal {
            "-TERM" => "tests::term_to_plain_mode_kills_the_running_bash_group",
            "-HUP" => "tests::hup_to_plain_mode_kills_the_running_bash_group",
            "-INT" => "tests::int_to_plain_mode_kills_the_running_bash_group",
            _ => panic!("unexpected signal {signal}"),
        };
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", test_name, "--nocapture"])
            .env("CUPEL_SIGNAL_CHILD", "1")
            .env("CUPEL_TEST_ROOT", &root)
            .env("CUPEL_PID_FILE", &pid_file)
            .stdin(Stdio::piped())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(b"run\n").unwrap();

        let deadline = Instant::now() + Duration::from_secs(10);
        while !pid_file.exists() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        if !pid_file.exists() {
            let _ = child.kill();
            panic!("bash never started");
        }
        let pid = std::fs::read_to_string(&pid_file)
            .unwrap()
            .trim()
            .to_string();
        assert!(
            Command::new("kill")
                .args(["-0", &pid])
                .output()
                .unwrap()
                .status
                .success(),
            "bash child {pid} was not running before {signal}"
        );
        assert!(
            Command::new("kill")
                .args([signal, &child.id().to_string()])
                .status()
                .unwrap()
                .success(),
            "could not send {signal} to cupel"
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = Command::new("kill").args(["-9", &pid]).status();
                let _ = child.kill();
                panic!(
                    "cupel did not exit on {signal}: {}",
                    std::fs::read_to_string(&log_path).unwrap()
                );
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(
            status.code(),
            Some(exit_code),
            "{}",
            std::fs::read_to_string(&log_path).unwrap()
        );
        let survived = Command::new("kill")
            .args(["-0", &pid])
            .output()
            .unwrap()
            .status
            .success();
        if survived {
            let _ = Command::new("kill").args(["-9", &pid]).status();
        }
        assert!(!survived, "bash child {pid} survived {signal}");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn term_to_plain_mode_kills_the_running_bash_group() {
        check_signal("-TERM", 143);
    }

    #[test]
    fn hup_to_plain_mode_kills_the_running_bash_group() {
        check_signal("-HUP", 129);
    }

    #[test]
    fn int_to_plain_mode_kills_the_running_bash_group() {
        check_signal("-INT", 130);
    }
}
