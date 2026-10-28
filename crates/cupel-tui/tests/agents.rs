//! Direct helper calls exercised without a terminal or paid provider.

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use cupel_agent::{Agent, AgentOptions};
    use cupel_coding_agent::modes::SessionMeta;
    use cupel_coding_agent::session::SessionRecorder;
    use cupel_core::event_stream::{AssistantMessageStream, assistant_message_channel};
    use cupel_core::provider::{Provider, Registry};
    use cupel_core::types::{
        Api, AssistantContent, AssistantMessage, Context, InputModality, Message, Model, ModelCost,
        StopReason, StreamOptions, TextContent, Usage, UserContentBody, now_ms,
    };
    use cupel_tui::app::{App, Status};
    use cupel_tui::transcript::Cell;
    use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

    struct Mock {
        contexts: Mutex<Vec<Context>>,
    }

    impl Provider for Mock {
        fn api(&self) -> &str {
            "mock"
        }
        fn stream(
            &self,
            model: &Model,
            context: Context,
            options: StreamOptions,
        ) -> AssistantMessageStream {
            let text = context
                .messages
                .iter()
                .rev()
                .find_map(|message| match message {
                    Message::User(user) => match &user.content {
                        UserContentBody::Text(text) => Some(text.clone()),
                        UserContentBody::Blocks(_) => None,
                    },
                    _ => None,
                })
                .unwrap_or_default();
            self.contexts.lock().unwrap().push(context);
            let (stream, sink) = assistant_message_channel();
            let model = model.clone();
            tokio::spawn(async move {
                let _ = sink.start();
                let stop = match text.as_str() {
                    "hang" => {
                        options.signal.unwrap_or_default().cancelled().await;
                        StopReason::Aborted
                    }
                    "fail" => StopReason::Error,
                    _ => StopReason::Stop,
                };
                let answer = format!("answer: {text}");
                let _ = sink.text_start(0);
                let _ = sink.text_delta(0, answer.clone());
                let _ = sink.text_end(0, answer.clone());
                let message = AssistantMessage {
                    content: vec![AssistantContent::Text(TextContent::plain(answer))],
                    api: model.api,
                    provider: model.provider,
                    model: model.id,
                    response_model: None,
                    response_id: None,
                    usage: Usage {
                        input: 10,
                        output: 5,
                        ..Usage::default()
                    },
                    stop_reason: stop,
                    error_message: (stop == StopReason::Error).then(|| "no credit".into()),
                    error_kind: None,
                    timestamp: now_ms(),
                };
                if stop == StopReason::Stop {
                    let _ = sink.done(stop, message);
                } else {
                    let _ = sink.error(stop, message);
                }
            });
            stream
        }
    }

    fn app() -> (App, Arc<Mock>) {
        let model = Model {
            id: "mock-model".into(),
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
        let cwd = std::env::temp_dir().join(format!(
            "cupel-tui-m4-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&cwd).unwrap();
        let mock = Arc::new(Mock {
            contexts: Mutex::new(Vec::new()),
        });
        let mut registry = Registry::new();
        registry.register(Arc::clone(&mock) as Arc<dyn Provider>);
        let settings = serde_json::from_value(serde_json::json!({
        "providers":{"mock":"key"},
        "agents":{"custom_42":{"provider":"mock","model":"mock-model","thinkingLevel":"off","description":"Finds code."}}
    })).unwrap();
        let recorder = SessionRecorder::new(None, &cwd, "test", "mock-model");
        let meta = SessionMeta {
            model_name: "Mock".into(),
            provider: "mock".into(),
            cwd: cwd.to_string_lossy().into_owned(),
            templates: Vec::new(),
            models: vec![model.clone()],
            settings,
            home: None,
            startup_warning: None,
            warnings: Vec::new(),
            context_files: Vec::new(),
            base_system_prompt: String::new(),
        };
        (
            App::new(
                Agent::new(AgentOptions::new(model, Arc::new(registry))),
                meta,
                recorder,
            ),
            mock,
        )
    }

    fn press(app: &mut App, key: KeyCode) {
        app.on_terminal_event(Event::Key(KeyEvent::new(key, KeyModifiers::NONE)));
    }

    fn submit(app: &mut App, text: &str) {
        app.input.insert_str(text);
        press(app, KeyCode::Enter);
    }

    async fn idle(app: &mut App) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while app.is_running() {
                let event = app.next_event().await;
                app.on_event(event).await;
            }
        })
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn direct_call_is_lazy_and_only_runs_the_helper() {
        let (mut app, mock) = app();
        submit(&mut app, "/team");
        assert!(!app.crew.is_built("custom_42"));
        assert!(mock.contexts.lock().unwrap().is_empty());
        submit(&mut app, "/agent custom_42 find the parser");
        idle(&mut app).await;
        assert_eq!(app.last_answer, Status::Done);
        assert!(app.run_finished);
        assert!(app.transcript.cells.iter().any(|cell| matches!(cell, Cell::Agent { name, text } if name == "custom_42" && text == "answer: find the parser")));
        assert_eq!(app.totals.input, 0);
        assert_eq!(app.session_totals().input, 10);
        assert_eq!(mock.contexts.lock().unwrap().len(), 1);
        app.start_run("lead task");
        idle(&mut app).await;
        let contexts = mock.contexts.lock().unwrap();
        assert!(contexts[1].messages.iter().any(|message| matches!(message, Message::User(user) if matches!(&user.content, UserContentBody::Text(text) if text.starts_with("[agent exchange] custom_42")))));
    }

    #[tokio::test]
    async fn arming_is_one_shot_and_esc_disarms() {
        let (mut app, _) = app();
        submit(&mut app, "/agent custom_42");
        assert_eq!(app.armed.as_deref(), Some("custom_42"));
        press(&mut app, KeyCode::Esc);
        assert!(app.armed.is_none());
        submit(&mut app, "/agent custom_42");
        submit(&mut app, "task");
        assert!(app.armed.is_none());
        idle(&mut app).await;
        assert_eq!(app.crew.total_spend().calls, 1);
    }

    #[tokio::test]
    async fn immediate_esc_works_before_the_worker_is_polled() {
        let (mut app, mock) = app();
        submit(&mut app, "/agent custom_42 hang");
        press(&mut app, KeyCode::Esc);
        idle(&mut app).await;
        assert_eq!(app.last_answer, Status::Cancelled);
        assert!(mock.contexts.lock().unwrap().is_empty());
        assert!(!app.crew.is_built("custom_42"));
    }

    #[tokio::test]
    async fn failure_is_preserved_for_the_lead_and_spend_survives_rebuild() {
        let (mut app, _) = app();
        submit(&mut app, "/agent custom_42 fail");
        idle(&mut app).await;
        assert_eq!(app.last_answer, Status::Failed);
        let state = app.agent.state();
        assert!(state.messages.iter().any(|message| matches!(message, cupel_agent::AgentMessage::Llm(Message::User(user)) if matches!(&user.content, UserContentBody::Text(text) if text.contains("no credit")))));
        let mut options = AgentOptions::new(app.meta.models[0].clone(), app.agent.registry());
        options.messages = state.messages;
        let restored = App::new(Agent::new(options), app.meta, app.recorder);
        assert!(!restored.crew.is_built("custom_42"));
        assert_eq!(restored.session_totals().input, 10);
    }

    #[tokio::test]
    async fn only_one_helper_can_wait_and_esc_restores_it() {
        let (mut app, _) = app();
        submit(&mut app, "/agent custom_42 hang");
        submit(&mut app, "/agent custom_42 waiting task");
        submit(&mut app, "/agent custom_42 third task");
        assert_eq!(app.waiting_helper.as_ref().unwrap().task, "waiting task");
        press(&mut app, KeyCode::Esc);
        idle(&mut app).await;
        assert!(app.input.text().contains("/agent custom_42 waiting task"));
        assert!(app.waiting_helper.is_none());
    }

    fn routed_app(target: &str) -> (App, Arc<Mock>) {
        let (app, mock) = app();
        let mut meta = app.meta;
        meta.templates
            .push(cupel_coding_agent::commands::PromptTemplate {
                name: "explain".into(),
                description: "Explains".into(),
                content: "Explain $1.".into(),
                path: "explain.md".into(),
            });
        meta.settings
            .agent_commands
            .insert("explain".into(), target.into());
        meta.settings
            .agent_commands
            .insert("review".into(), target.into());
        (App::new(app.agent, meta, app.recorder), mock)
    }

    #[tokio::test]
    async fn explicit_template_routes_run_without_a_lead_turn() {
        let (mut app, mock) = routed_app("custom_42");
        submit(&mut app, "/explain parser");
        idle(&mut app).await;
        assert_eq!(app.crew.total_spend().calls, 1);
        assert_eq!(app.totals.input, 0);
        assert_eq!(mock.contexts.lock().unwrap().len(), 1);
        assert!(app.transcript.cells.iter().any(
            |cell| matches!(cell, Cell::Agent { text, .. } if text.contains("Explain parser."))
        ));
    }

    #[tokio::test]
    async fn invalid_explicit_mapping_falls_back_to_lead_not_armed_helper() {
        let (mut app, mock) = routed_app("missing");
        assert!(app.routes.is_empty());
        submit(&mut app, "/agent custom_42");
        submit(&mut app, "/explain parser");
        assert!(app.helper.is_none());
        assert_eq!(app.pending_prompt.as_deref(), Some("Explain parser."));
        assert_eq!(app.armed.as_deref(), Some("custom_42"));
        assert!(mock.contexts.lock().unwrap().is_empty());
    }

    #[test]
    fn builtin_shadowing_templates_cannot_acquire_routes() {
        let (mut app, _) = routed_app("custom_42");
        app.meta
            .templates
            .push(cupel_coding_agent::commands::PromptTemplate {
                name: "model".into(),
                description: String::new(),
                content: "shadow".into(),
                path: "model.md".into(),
            });
        app.meta
            .settings
            .agent_commands
            .insert("model".into(), "custom_42".into());
        let (routes, warnings) =
            cupel_coding_agent::crew::routes(&app.meta.settings, &app.crew, &app.meta.templates);
        assert!(!routes.contains_key("model"));
        assert_eq!(warnings.len(), 1);
        assert!(!app.crew.is_built("custom_42"));
    }

    #[tokio::test]
    async fn team_membership_changes_tools_without_constructing_agents() {
        let (mut app, mock) = app();
        submit(&mut app, "/team add missing custom_42");
        assert_eq!(app.team.iter().cloned().collect::<Vec<_>>(), ["custom_42"]);
        assert!(!app.crew.is_built("custom_42"));
        assert!(mock.contexts.lock().unwrap().is_empty());
        let ask = Arc::clone(
            app.agent
                .tools()
                .iter()
                .find(|tool| tool.name() == "ask")
                .unwrap(),
        );
        assert_eq!(ask.execution_mode(), None);
        assert!(
            app.agent
                .tools()
                .iter()
                .all(|tool| tool.name() != "delegate")
        );
        assert!(ask.description().contains("otherwise do the work yourself"));
        assert_eq!(
            ask.parameters()["properties"]["agent"]["enum"],
            serde_json::json!(["custom_42"])
        );
        for args in [
            serde_json::json!({"agent":"missing","task":"task"}),
            serde_json::json!({"agent":"custom_42","task":" "}),
        ] {
            assert!(
                ask.execute(
                    "bad",
                    args,
                    tokio_util::sync::CancellationToken::new(),
                    None
                )
                .await
                .is_err()
            );
        }
        assert!(!app.crew.is_built("custom_42"));
        ask.execute(
            "ok",
            serde_json::json!({"agent":"custom_42","task":"task"}),
            tokio_util::sync::CancellationToken::new(),
            None,
        )
        .await
        .unwrap();
        assert!(app.crew.is_built("custom_42"));
        submit(&mut app, "/team clear");
        assert!(app.team.is_empty());
        assert!(app.agent.tools().iter().all(|tool| tool.name() != "ask"));
        let mut options = AgentOptions::new(app.meta.models[0].clone(), app.agent.registry());
        options.messages = app.agent.state().messages;
        let restored = App::new(Agent::new(options), app.meta, app.recorder);
        assert!(restored.team.is_empty(), "saved empty membership wins");
    }

    #[test]
    fn team_restoration_revalidates_eligibility_and_write_permissions() {
        let (app, _) = app();
        let mut meta = app.meta;
        meta.settings.agents.get_mut("custom_42").unwrap().tools =
            Some(vec![cupel_coding_agent::settings::ToolName::Bash]);
        let mut manual = meta.settings.agents["custom_42"].clone();
        manual.description = None;
        meta.settings.agents.insert("manual".into(), manual);
        let mut options = AgentOptions::new(meta.models[0].clone(), app.agent.registry());
        options.messages = vec![cupel_agent::AgentMessage::Custom {
            kind: "team".into(),
            payload: serde_json::json!({"members":["custom_42","manual","missing"]}),
            timestamp: now_ms(),
        }];
        let restored = App::new(Agent::new(options), meta, app.recorder);
        assert_eq!(
            restored.team.iter().cloned().collect::<Vec<_>>(),
            ["custom_42"]
        );
        let tools = restored.agent.tools();
        let delegate = tools.iter().find(|tool| tool.name() == "delegate").unwrap();
        assert_eq!(
            delegate.execution_mode(),
            Some(cupel_agent::ToolExecutionMode::Sequential)
        );
        assert!(tools.iter().all(|tool| tool.name() != "ask"));
        assert!(!restored.crew.is_built("custom_42"));
    }

    #[test]
    fn team_command_forms_are_strict() {
        use cupel_coding_agent::commands::TeamCommand;
        assert_eq!("".parse::<TeamCommand>(), Ok(TeamCommand::List));
        assert_eq!("clear".parse::<TeamCommand>(), Ok(TeamCommand::Clear));
        assert_eq!(
            "add one two".parse::<TeamCommand>(),
            Ok(TeamCommand::Add(vec!["one".into(), "two".into()]))
        );
        assert_eq!(
            "remove one".parse::<TeamCommand>(),
            Ok(TeamCommand::Remove(vec!["one".into()]))
        );
        for invalid in ["add", "remove", "clear extra", "drop one"] {
            assert!(invalid.parse::<TeamCommand>().is_err());
        }
    }

    #[tokio::test]
    async fn changing_team_invalidates_provider_context_usage_anchors() {
        let (mut app, mock) = app();
        app.start_run("first");
        idle(&mut app).await;
        submit(&mut app, "/team add custom_42");
        app.start_run("second");
        idle(&mut app).await;
        let contexts = mock.contexts.lock().unwrap();
        let prior = contexts[1]
            .messages
            .iter()
            .find_map(|message| match message {
                Message::Assistant(assistant) => Some(assistant),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            prior.usage.input, 0,
            "old size measured a different tool schema"
        );
        assert_eq!(
            app.totals.input, 20,
            "actual incurred usage remains accounted"
        );
    }

    fn default_team_app() -> (App, Arc<Mock>) {
        let (app, mock) = app();
        let mut meta = app.meta;
        meta.settings.agents.get_mut("custom_42").unwrap().team = Some(true);
        (App::new(app.agent, meta, app.recorder), mock)
    }

    #[tokio::test]
    async fn defaults_are_lazy_and_deferred_until_interaction() {
        use cupel_tui::startup::{StartupCommand, TeamInitialization};
        let (mut app, mock) = default_team_app();
        app.initialize_team(TeamInitialization::ConfiguredDefaults);
        assert!(app.team.contains("custom_42"));
        assert!(
            app.agent.state().messages.is_empty(),
            "launching and quitting leaves no transcript"
        );
        assert!(!app.crew.is_built("custom_42"));
        app.apply_startup(StartupCommand::Agent {
            name: "custom_42".into(),
            task: Some("task".into()),
        });
        idle(&mut app).await;
        assert_eq!(mock.contexts.lock().unwrap().len(), 1);
        assert!(app.agent.state().messages.iter().any(|message| matches!(message, cupel_agent::AgentMessage::Custom { kind, .. } if kind == "team")));
    }

    #[test]
    fn restoration_without_a_saved_team_does_not_apply_new_defaults() {
        use cupel_tui::startup::TeamInitialization;
        let (mut app, _) = default_team_app();
        app.initialize_team(TeamInitialization::Restore);
        assert!(app.team.is_empty());
        assert!(app.agent.state().messages.is_empty());
    }

    #[test]
    fn saved_empty_team_beats_configured_defaults() {
        use cupel_tui::startup::TeamInitialization;
        let (mut app, _) = default_team_app();
        app.initialize_team(TeamInitialization::ConfiguredDefaults);
        submit(&mut app, "/team clear");
        let mut options = AgentOptions::new(app.meta.models[0].clone(), app.agent.registry());
        options.messages = app.agent.state().messages;
        let mut restored = App::new(Agent::new(options), app.meta, app.recorder);
        restored.initialize_team(TeamInitialization::ConfiguredDefaults);
        assert!(restored.team.is_empty());
    }
}
