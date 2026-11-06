//! Single-agent regression tests using real tools and a scripted provider.

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use cupel_agent::{Agent, AgentMessage, AgentOptions};
    use cupel_coding_agent::bootstrap;
    use cupel_coding_agent::modes::SessionMeta;
    use cupel_coding_agent::resources::ContextFile;
    use cupel_coding_agent::session::{SessionRecorder, load_transcript};
    use cupel_coding_agent::settings::Settings;
    use cupel_core::event_stream::{AssistantMessageStream, assistant_message_channel};
    use cupel_core::provider::{Provider, Registry};
    use cupel_core::types::{
        Api, AssistantContent, AssistantMessage, Context, Message, Model, StopReason,
        StreamOptions, TextContent, ToolCall, ToolResultContent, Usage,
    };
    use cupel_tui::app::{App, Status};
    use serde_json::{Value, json};

    // Separate processes measure real BashTool overlap, independently of UI events.
    const OVERLAP: &str = r"import fcntl, json, pathlib, sys, time
name = sys.argv[1]
def update(delta):
    with open('overlap.lock', 'a') as lock:
        fcntl.flock(lock, fcntl.LOCK_EX)
        path = pathlib.Path('overlap.json')
        state = json.loads(path.read_text()) if path.exists() else {'active': 0, 'peak': 0, 'calls': 0}
        state['active'] += delta
        if delta > 0:
            state['calls'] += 1
            state['peak'] = max(state['peak'], state['active'])
        path.write_text(json.dumps(state, sort_keys=True))
update(1)
try:
    time.sleep(0.1)
    pathlib.Path(name + '.txt').write_text(name + '-artifact\n')
    print(name + '-output')
finally:
    update(-1)
";

    struct Script {
        requests: Mutex<Vec<Value>>,
    }

    impl Provider for Script {
        fn api(&self) -> &str {
            "solo-observation"
        }

        fn stream(
            &self,
            model: &Model,
            context: Context,
            options: StreamOptions,
        ) -> AssistantMessageStream {
            let mut requests = self.requests.lock().unwrap();
            let turn = requests.len();
            let StreamOptions {
                temperature,
                max_tokens,
                api_key,
                signal,
                cache_retention,
                session_id,
                headers,
                timeout_ms,
                max_retries,
                metadata,
                env,
                reasoning,
                thinking_budgets,
            } = options;
            assert!(
                api_key.as_deref() == Some("fixture-never-sent-over-network"),
                "the synthetic transport credential changed"
            );
            requests.push(json!({
                "model": model, "context": context,
                "options": {
                    "temperature": temperature, "max_tokens": max_tokens,
                    "api_key_present": api_key.is_some(),
                    "signal": signal.as_ref().map(|signal| json!({"cancelled": signal.is_cancelled()})),
                    "cache_retention": cache_retention, "session_id": session_id,
                    "headers": headers, "timeout_ms": timeout_ms, "max_retries": max_retries,
                    "metadata": metadata, "env": env, "reasoning": reasoning,
                    "thinking_budgets": thinking_budgets,
                },
            }));
            assert!(
                turn < 2,
                "unexpected provider request: possible retry or loop"
            );
            let (content, stop_reason) = if turn == 0 {
                (
                    ["alpha", "beta"]
                        .into_iter()
                        .map(|name| {
                            AssistantContent::ToolCall(ToolCall {
                                id: name.into(),
                                name: "bash".into(),
                                arguments: json!({
                                    "command": format!("python3 overlap.py {name}"),
                                    "timeout": 10,
                                }),
                            })
                        })
                        .collect(),
                    StopReason::ToolUse,
                )
            } else {
                (
                    vec![AssistantContent::Text(TextContent::plain("BATCH_COMPLETE"))],
                    StopReason::Stop,
                )
            };
            let message = AssistantMessage {
                content,
                api: model.api.clone(),
                provider: model.provider.clone(),
                model: model.id.clone(),
                response_model: None,
                response_id: None,
                usage: Usage::default(),
                stop_reason,
                error_message: None,
                error_kind: None,
                timestamp: 1,
            };
            let (stream, sink) = assistant_message_channel();
            assert!(sink.start());
            assert!(sink.done(stop_reason, message));
            stream
        }
    }

    fn ingredients(
        root: &Path,
        registry: Arc<Registry>,
        configured: bool,
        messages: Vec<AgentMessage>,
    ) -> (Agent, SessionMeta, SessionRecorder) {
        let cwd = root.join("project");
        let home = root.join("home");
        let mut model = cupel_core::catalog::builtin_models().remove(0);
        model.id = "solo-model".into();
        model.api = Api::from("solo-observation");
        model.provider = cupel_core::types::Provider::from("solo-provider");
        model.reasoning = false;
        model.thinking_level_map = None;
        let settings: Settings = if configured {
            // Removed configuration stays opaque, even with automatic membership
            // and command routing enabled in an old settings file.
            serde_json::from_value(json!({
                "agents": {"reader": {
                    "provider": "solo-provider", "model": "solo-model",
                    "thinkingLevel": "off", "description": "Read-only fixture peer",
                    "tools": ["read", "grep"], "team": true,
                }},
                "agentCommands": {"review": "reader"},
            }))
            .unwrap()
        } else {
            Settings::default()
        };
        let context_files = vec![ContextFile {
            path: cwd.join("AGENTS.md"),
            content: "CONTEXT_FILE_MARKER: preserve the fixture's public API.\n".into(),
        }];
        let base = cupel_coding_agent::system_prompt::build_system_prompt(
            &cwd,
            bootstrap::TOOL_SUMMARIES,
            &context_files,
        );
        let mut options = AgentOptions::new(model.clone(), registry);
        options.tool_execution = cupel_agent::ToolExecutionMode::Parallel;
        options.api_key = Some("fixture-never-sent-over-network".into());
        options.session_id = Some("solo-fixture-session".into());
        options.temperature = Some(0.25);
        options.max_tokens = Some(1024);
        options.system_prompt = base.clone();
        options.tools = bootstrap::coding_tools(&cwd);
        options.messages = messages;
        options.retry.max_retries = 0;
        let meta = SessionMeta {
            model_name: model.name.clone(),
            provider: model.provider.as_str().into(),
            cwd: cwd.to_string_lossy().into_owned(),
            templates: Vec::new(),
            models: vec![model.clone()],
            settings,
            home: Some(home.clone()),
            startup_warning: None,
            warnings: Vec::new(),
            context_files,
            base_system_prompt: base,
        };
        (
            Agent::new(options),
            meta,
            SessionRecorder::new(Some(home), &cwd, "solo-fixture", &model.id),
        )
    }

    fn app(
        root: &Path,
        registry: Arc<Registry>,
        configured: bool,
        messages: Vec<AgentMessage>,
    ) -> App {
        let (agent, meta, recorder) = ingredients(root, registry, configured, messages);
        App::new(agent, meta, recorder)
    }

    fn render(app: &mut App) {
        let backend = ratatui::backend::TestBackend::new(120, 40);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                cupel_tui::ui::render(frame, app, frame.area(), &cupel_tui::theme::CUPEL);
            })
            .unwrap();
    }

    async fn batch(app: &mut App) {
        app.recorder
            .before_prompt("Run the independent alpha and beta observations.")
            .await;
        app.start_run("Run the independent alpha and beta observations.");
        tokio::time::timeout(Duration::from_secs(30), async {
            while app.is_running() {
                let event = app.next_event().await;
                app.on_event(event).await;
            }
        })
        .await
        .expect("App batch timed out");
        assert_eq!(app.last_answer, Status::Done, "model/App run failed");
    }

    async fn observe(root: &Path, case: &str) -> Value {
        assert!(["fresh", "configured", "resumed"].contains(&case));
        assert!(!root.exists(), "fixture root must be fresh");
        std::fs::create_dir_all(root.join("project")).unwrap();
        std::fs::create_dir_all(root.join("home")).unwrap();
        std::fs::write(root.join("project/overlap.py"), OVERLAP).unwrap();
        let script = Arc::new(Script {
            requests: Mutex::new(Vec::new()),
        });
        let mut registry = Registry::new();
        registry.register(Arc::clone(&script) as Arc<dyn Provider>);
        let registry = Arc::new(registry);
        let mut app = app(root, Arc::clone(&registry), case != "fresh", Vec::new());
        if case != "fresh" {
            let private =
                AgentMessage::user_text("PRIVATE_HISTORY_MARKER: previous accepted task.");
            app.recorder.record(&private);
            app.agent.push_message(private);
        }
        if case == "resumed" {
            let path = app.recorder.transcript().unwrap().to_path_buf();
            let (_, history) = load_transcript(&path).unwrap();
            assert_eq!(history, app.agent.state().messages);
            app.recorder.end_session().await;
            drop(app);
            app = self::app(root, Arc::clone(&registry), true, history);
        }
        render(&mut app);
        let tools_before = app.agent.tools().iter().map(|tool| json!({
            "name": tool.name(), "description": tool.description(), "parameters": tool.parameters(),
        })).collect::<Vec<_>>();
        // These public reads are also exercised by the footer without a run.
        let _ = app.session_totals();
        batch(&mut app).await;
        if matches!(case, "fresh" | "configured") {
            app.recorder
                .before_prompt("Abort this before inference.")
                .await;
            app.start_run("Abort this before inference.");
            app.on_terminal_event(ratatui::crossterm::event::Event::Key(
                ratatui::crossterm::event::KeyEvent::new(
                    ratatui::crossterm::event::KeyCode::Esc,
                    ratatui::crossterm::event::KeyModifiers::NONE,
                ),
            ));
            while app.is_running() {
                let event = app.next_event().await;
                app.on_event(event).await;
            }
            assert_eq!(app.last_answer, Status::Cancelled);
            let _ = (app.session_totals(), app.activity_label());
        }
        let requests = script.requests.lock().unwrap().clone();
        assert_eq!(
            requests.len(),
            2,
            "aborting must not make another provider request"
        );
        let history = app.agent.state().messages;
        let tool_results = history
            .iter()
            .filter_map(|message| match message {
                AgentMessage::Llm(Message::ToolResult(result)) => {
                    assert!(!result.is_error, "real tool execution failed: {result:?}");
                    assert_eq!(
                        result.content,
                        vec![ToolResultContent::Text(TextContent::plain(format!(
                            "{}-output\n",
                            result.tool_call_id
                        )))],
                        "unexpected real tool output"
                    );
                    Some(serde_json::to_value(result).unwrap())
                }
                AgentMessage::Llm(Message::Assistant(message)) => {
                    assert!(
                        message.error_message.is_none()
                            || (matches!(case, "fresh" | "configured")
                                && message.stop_reason == StopReason::Aborted
                                && message.error_message.as_deref() == Some("request was aborted")),
                        "unexpected provider error: {message:?}"
                    );
                    None
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(tool_results.len(), 2);
        for request in &requests {
            let request_text = serde_json::to_string(request).unwrap();
            assert!(request_text.contains("CONTEXT_FILE_MARKER"));
            let tools = request["context"]["tools"]
                .as_array()
                .unwrap()
                .iter()
                .map(|tool| tool["name"].as_str().unwrap())
                .collect::<Vec<_>>();
            assert_eq!(tools, ["read", "bash", "apply_patch", "grep"]);
            if case != "fresh" {
                assert!(request_text.contains("PRIVATE_HISTORY_MARKER"));
            }
        }
        let overlap: Value =
            serde_json::from_slice(&std::fs::read(root.join("project/overlap.json")).unwrap())
                .unwrap();
        assert_eq!(overlap["active"], 0);
        assert_eq!(overlap["calls"], 2);
        let artifacts = json!({
            "alpha.txt": std::fs::read_to_string(root.join("project/alpha.txt")).unwrap(),
            "beta.txt": std::fs::read_to_string(root.join("project/beta.txt")).unwrap(),
        });
        assert_eq!(
            artifacts,
            json!({"alpha.txt": "alpha-artifact\n", "beta.txt": "beta-artifact\n"})
        );
        render(&mut app);
        app.recorder.end_session().await;
        let (_, persisted) = load_transcript(app.recorder.transcript().unwrap()).unwrap();
        assert_eq!(persisted, history);
        assert_eq!(
            tools_before
                .iter()
                .map(|tool| tool["name"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["read", "bash", "apply_patch", "grep"]
        );
        json!({"overlap": overlap})
    }

    struct OwnedRoot(PathBuf);

    impl OwnedRoot {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "cupel-solo-regression-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            // Claim ownership atomically; never delete or reuse an existing path.
            #[expect(
                clippy::create_dir,
                reason = "atomic fixture ownership must reject existing directories"
            )]
            std::fs::create_dir(&root).unwrap();
            Self(root)
        }
    }

    impl Drop for OwnedRoot {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn solo_path_regression() {
        let root = OwnedRoot::new();
        for case in ["fresh", "configured", "resumed"] {
            let observation = observe(&root.0.join(case), case).await;
            assert_eq!(
                observation["overlap"]["peak"], 2,
                "unexpected tool concurrency for {case}"
            );
        }
    }
}
