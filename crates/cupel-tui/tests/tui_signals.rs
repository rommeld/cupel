//! Real terminal shutdown, without network requests. The PTY driver uses Python's
//! standard library so no unsafe terminal bindings are needed in the Rust tests.

#[cfg(test)]
#[cfg(unix)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;
    use std::path::Path;
    use std::process::Command;
    use std::sync::Arc;

    use cupel_agent::{Agent, AgentOptions};
    use cupel_coding_agent::modes::SessionMeta;
    use cupel_coding_agent::session::SessionRecorder;
    use cupel_coding_agent::settings::Settings;
    use cupel_coding_agent::tools::bash::BashTool;
    use cupel_core::event_stream::{AssistantMessageStream, assistant_message_channel};
    use cupel_core::provider::{Provider, Registry};
    use cupel_core::types::{
        Api, AssistantContent, AssistantMessage, Context, Model, StopReason, StreamOptions,
        ToolCall, Usage, now_ms,
    };

    struct MockProvider;

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
            let _ = sink.done(
                StopReason::ToolUse,
                AssistantMessage {
                    content: vec![AssistantContent::ToolCall(ToolCall {
                        id: "sleep".into(),
                        name: "bash".into(),
                        arguments: serde_json::json!({
                            "command": "echo $$ > shell.pid; exec sleep 90"
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
                    error_kind: None,
                    timestamp: now_ms(),
                },
            );
            stream
        }
    }

    async fn run_child(root: &Path) -> std::io::Result<()> {
        let mut model = cupel_core::catalog::builtin_models().remove(0);
        model.api = Api::from("mock");
        model.provider = cupel_core::types::Provider::from("mock");
        let mut registry = Registry::new();
        registry.register(Arc::new(MockProvider));
        let mut options = AgentOptions::new(model.clone(), Arc::new(registry));
        options.tools = vec![Arc::new(BashTool::new(root))];
        let home = root.join("home");
        let recorder = SessionRecorder::new(Some(home.clone()), root, "signal-test", "mock");
        let meta = SessionMeta {
            model_name: "Mock".into(),
            provider: "mock".into(),
            cwd: root.display().to_string(),
            templates: Vec::new(),
            models: vec![model],
            home: Some(home),
            settings: Settings::default(),
            startup_warning: None,
            warnings: Vec::new(),
            context_files: Vec::new(),
            base_system_prompt: String::new(),
        };
        cupel_tui::run(Agent::new(options), meta, recorder).await
    }

    fn check_shutdown(test_name: &str, signal: &str, idle: bool) {
        if let Some(root) = std::env::var_os("CUPEL_TUI_SIGNAL_CHILD") {
            let result = tokio::runtime::Runtime::new()
                .unwrap()
                .block_on(run_child(Path::new(&root)));
            std::process::exit(i32::from(result.is_err()));
        }

        let root = std::env::temp_dir().join(format!(
            "cupel-tui-signal-{}-{test_name}-{}",
            std::process::id(),
            now_ms()
        ));
        let hooks = root.join("home/hooks/session-end");
        std::fs::create_dir_all(&hooks).unwrap();
        let script = hooks.join("record");
        std::fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf 'ended\\n' >> '{}'\n",
                root.join("session-ended").display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let output = Command::new("python3")
            .arg("-c")
            .arg(include_str!("support/tui_signals.py"))
            .arg(std::env::current_exe().unwrap())
            .arg(test_name)
            .arg(&root)
            .arg(signal)
            .arg(if idle { "idle" } else { "running" })
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "PTY shutdown failed: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        if idle {
            assert!(
                !root.join("session-ended").exists(),
                "an unused session must not announce session-end"
            );
        } else {
            assert_eq!(
                std::fs::read_to_string(root.join("session-ended")).unwrap(),
                "ended\n",
                "session-end must fire exactly once"
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn term_cleans_up_the_terminal_and_running_bash_group() {
        check_shutdown(
            "tests::term_cleans_up_the_terminal_and_running_bash_group",
            "SIGTERM",
            false,
        );
    }

    #[test]
    fn hup_cleans_up_the_terminal_and_running_bash_group() {
        check_shutdown(
            "tests::hup_cleans_up_the_terminal_and_running_bash_group",
            "SIGHUP",
            false,
        );
    }

    #[test]
    fn int_cleans_up_the_terminal_and_running_bash_group() {
        check_shutdown(
            "tests::int_cleans_up_the_terminal_and_running_bash_group",
            "SIGINT",
            false,
        );
    }

    #[test]
    fn term_restores_an_idle_terminal_without_firing_session_end() {
        check_shutdown(
            "tests::term_restores_an_idle_terminal_without_firing_session_end",
            "SIGTERM",
            true,
        );
    }

    #[test]
    fn closing_the_terminal_kills_bash_and_fires_session_end() {
        check_shutdown(
            "tests::closing_the_terminal_kills_bash_and_fires_session_end",
            "close",
            false,
        );
    }
}
