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
                        UserContentBody::Text(text)
                            if !text.starts_with("[Task ")
                                && !text.starts_with("[Shared team conversation") =>
                        {
                            Some(text.clone())
                        }
                        UserContentBody::Text(_) | UserContentBody::Blocks(_) => None,
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
        app.crew
            .lead_context()
            .set(cupel_coding_agent::team_scheduler::RunContext::root(
                cupel_coding_agent::team_conversation::Participant::Lead,
                cupel_coding_agent::team_scheduler::AccessScope::CanWrite,
                "test parent".into(),
                tokio_util::sync::CancellationToken::new(),
            ));
        ask.execute(
            "ok",
            serde_json::json!({"agent":"custom_42","task":"task"}),
            tokio_util::sync::CancellationToken::new(),
            None,
        )
        .await
        .unwrap();
        assert!(app.crew.is_built("custom_42"));
        if let Some(run) = app.crew.lead_context().current() {
            let scope = run.scope(&app.crew.scheduler()).await.unwrap();
            app.crew
                .scheduler()
                .finish(
                    &scope,
                    cupel_coding_agent::team_scheduler::TaskState::Succeeded,
                    "done",
                    cupel_coding_agent::crew::Changes::ReadOnly,
                )
                .await;
            app.crew.lead_context().clear();
        }
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

    #[tokio::test]
    async fn on_team_results_use_the_thread_once_and_remain_visible_after_clear() {
        use cupel_coding_agent::team_conversation::{Participant, TeamBody};
        use cupel_coding_agent::team_scheduler::TaskState;
        let (mut app, mock) = app();
        submit(&mut app, "/team add custom_42");
        submit(&mut app, "/agent custom_42 shared task");
        idle(&mut app).await;
        let page = app
            .team_runtime
            .read(&Participant::Lead, None)
            .await
            .unwrap();
        assert_eq!(page.entries.len(), 1);
        assert_eq!(
            page.entries[0].author,
            Participant::Helper("custom_42".into())
        );
        assert!(matches!(
            &page.entries[0].body,
            TeamBody::Scheduled { task } if task.state == TaskState::Succeeded
        ));
        assert!(!app.agent.state().messages.iter().any(|message| matches!(message, cupel_agent::AgentMessage::Llm(Message::User(user)) if matches!(&user.content, UserContentBody::Text(text) if text.starts_with("[agent exchange]")))));
        {
            let contexts = mock.contexts.lock().unwrap();
            let tools = contexts[0].tools.as_ref().unwrap();
            assert!(tools.iter().any(|tool| tool.name == "team_post"));
            assert!(tools.iter().any(|tool| tool.name == "team_read"));
        }
        submit(&mut app, "/team clear");
        app.start_run("what did the helper find?");
        idle(&mut app).await;
        let contexts = mock.contexts.lock().unwrap();
        assert!(contexts[1].messages.iter().any(|message| matches!(message, Message::User(user) if matches!(&user.content, UserContentBody::Text(text) if text.contains("Shared team conversation") && text.contains("shared task")))));
        assert!(
            contexts[1]
                .tools
                .iter()
                .flatten()
                .all(|tool| tool.name != "team_post")
        );
    }

    fn rendered_text(app: &App) -> String {
        app.transcript
            .to_lines(160, app.selected_cell, &cupel_tui::theme::CUPEL)
            .lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[tokio::test]
    async fn team_final_answer_follows_named_lifecycle_and_late_helper_updates() {
        use cupel_coding_agent::team_conversation::Participant;
        let (mut app, _) = app();
        submit(&mut app, "/team add custom_42");
        app.start_run("lead task");
        idle(&mut app).await;
        let text = rendered_text(&app);
        assert!(text.contains("lead · mock/mock-model] succeeded"), "{text}");
        assert_eq!(
            text.matches("answer: lead task").count(),
            1,
            "no duplicate root result"
        );
        assert!(text.rfind("[team #").unwrap() < text.find("answer: lead task").unwrap());
        assert!(text.find("answer: lead task").unwrap() < text.find("worked for").unwrap());
        let _active = app.team_runtime.enter("custom_42");
        app.team_runtime
            .post(
                Participant::Helper("custom_42".into()),
                "late helper note".into(),
                None,
            )
            .await
            .unwrap();
        let event = tokio::time::timeout(Duration::from_secs(5), app.next_event())
            .await
            .unwrap();
        app.on_event(event).await;
        let text = rendered_text(&app);
        assert!(text.contains("@custom_42]"), "{text}");
        assert!(text.find("late helper note").unwrap() < text.find("answer: lead task").unwrap());
    }

    #[tokio::test]
    async fn lead_model_labels_keep_known_provenance_and_do_not_guess_on_replay() {
        use cupel_coding_agent::team_conversation::{Participant, TeamRuntime, TeamUpdate};
        use cupel_tui::app::AppEvent;
        let (mut source, _) = app();
        submit(&mut source, "/team add custom_42");
        source.start_run("model A task");
        idle(&mut source).await;
        let mut entry = source
            .team_runtime
            .inspect(None)
            .await
            .unwrap()
            .entries
            .pop()
            .unwrap();
        assert_eq!(entry.author, Participant::Lead);
        assert!(
            !entry.display().contains("lead ·"),
            "presentation must not change stored identity"
        );
        let mut model_b = source.agent.state().model;
        model_b.id = "model-b".into();
        source.agent.set_model(model_b.clone());
        entry.id += 1;
        source
            .on_event(AppEvent::Team(Some(TeamUpdate::Entry(entry.clone()))))
            .await;
        let text = rendered_text(&source);
        assert!(text.contains("lead · mock/mock-model] succeeded"), "{text}");
        assert!(!text.contains("lead · mock/model-b"), "{text}");

        let (mut restored, _) = app();
        restored.agent.set_model(model_b);
        let path = std::path::Path::new(&restored.meta.cwd).join("conversation.jsonl");
        std::fs::write(
            &path,
            format!(
                "{{\"schemaVersion\":1}}\n{}\n",
                serde_json::to_string(&entry).unwrap()
            ),
        )
        .unwrap();
        let (runtime, events) = TeamRuntime::new(Some(path));
        restored.team_runtime = runtime;
        restored.team_events = Some(events);
        let page = restored.team_runtime.inspect(None).await.unwrap();
        assert_eq!(page.entries[0].author, Participant::Lead);
        let event = tokio::time::timeout(Duration::from_secs(5), restored.next_event())
            .await
            .unwrap();
        restored.on_event(event).await;
        let text = rendered_text(&restored);
        assert!(
            text.contains("lead] succeeded"),
            "unknown historical model stays unknown: {text}"
        );
        assert!(!text.contains("lead ·"), "{text}");
    }

    #[tokio::test]
    async fn task_progress_revises_one_compact_cell_instead_of_repeating_handoffs() {
        use cupel_coding_agent::team_conversation::{TeamBody, TeamUpdate};
        use cupel_coding_agent::team_scheduler::TaskState;
        use cupel_tui::app::AppEvent;
        let (mut app, _) = app();
        submit(&mut app, "/team add custom_42");
        app.start_run("task");
        idle(&mut app).await;
        let before = app.team_runtime.inspect(None).await.unwrap().entries;
        let mut entry = before.last().unwrap().clone();
        let TeamBody::Scheduled { task } = &mut entry.body else {
            panic!("expected scheduled task")
        };
        task.state = TaskState::Running;
        task.task = "Review the implementation\n[Handoff context: private-looking noise]\nlong repeated instructions".into();
        task.summary.clear();
        let count = app.transcript.cells.len();
        for _ in 0..4 {
            entry.id += 1;
            app.on_event(AppEvent::Team(Some(TeamUpdate::Entry(entry.clone()))))
                .await;
        }
        let text = rendered_text(&app);
        assert_eq!(
            app.transcript.cells.len(),
            count,
            "progress updates replace the existing task cell"
        );
        assert_eq!(text.matches("Review the implementation").count(), 1);
        assert!(!text.contains("Handoff context") && !text.contains("long repeated instructions"));
        assert_eq!(
            app.team_runtime.inspect(None).await.unwrap().entries,
            before,
            "UI compaction must not rewrite shared records"
        );
    }

    #[tokio::test]
    async fn ctrl_b_hides_existing_and_future_team_updates_without_losing_history() {
        use cupel_coding_agent::team_conversation::TeamUpdate;
        use cupel_coding_agent::team_scheduler::CallId;
        use cupel_tui::app::AppEvent;
        let (mut app, _) = app();
        app.transcript.append_assistant("visible answer");
        app.transcript.promote_final_answer();
        let update = |tool: &str| {
            AppEvent::Team(Some(TeamUpdate::ToolStarted {
                helper: "custom_42".into(),
                tool: tool.into(),
                call: CallId(2),
                root: CallId(1),
            }))
        };
        app.on_event(update("grep pattern src/")).await;
        assert!(rendered_text(&app).contains("@custom_42] call=2 root=1\ntool: grep pattern src/"));
        app.selected_cell = Some(1);
        app.on_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::Char('b'),
            KeyModifiers::CONTROL,
        )));
        assert!(app.transcript.hide_team);
        assert_eq!(app.selected_cell, None);
        app.on_event(update("read note.md:1-2")).await;
        let hidden = rendered_text(&app);
        assert!(hidden.contains("visible answer"));
        assert!(!hidden.contains("tool:"));
        assert_eq!(app.transcript.cells.len(), 3);
        assert_eq!(app.transcript.copy_text(0), Some("visible answer"));
        app.selected_cell = Some(0);
        app.on_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::Char('b'),
            KeyModifiers::CONTROL,
        )));
        assert!(!app.transcript.hide_team);
        assert_eq!(app.selected_cell, Some(0));
        let shown = rendered_text(&app);
        assert!(
            shown.contains("tool: grep pattern src/") && shown.contains("tool: read note.md:1-2")
        );
        assert!(
            shown.find("tool: read note.md:1-2").unwrap() < shown.find("visible answer").unwrap()
        );
        let rendered = app
            .transcript
            .to_lines(160, Some(0), &cupel_tui::theme::CUPEL);
        assert_eq!(rendered.cell_at.last(), Some(&Some(0)));
    }

    #[tokio::test]
    async fn shared_posting_is_not_a_scheduler_and_removed_ask_tools_are_revoked() {
        use cupel_coding_agent::team_conversation::Participant;
        let (mut app, mock) = app();
        submit(&mut app, "/team add custom_42");
        let ask = Arc::clone(
            app.agent
                .tools()
                .iter()
                .find(|tool| tool.name() == "ask")
                .unwrap(),
        );
        app.team_runtime
            .post(Participant::Lead, "an attributed finding".into(), None)
            .await
            .unwrap();
        assert!(mock.contexts.lock().unwrap().is_empty());
        assert!(!app.crew.is_built("custom_42"));
        submit(&mut app, "/team clear");
        assert!(
            ask.execute(
                "removed",
                serde_json::json!({"agent":"custom_42","task":"task"}),
                tokio_util::sync::CancellationToken::new(),
                None
            )
            .await
            .is_err()
        );
        submit(&mut app, "/agent custom_42 private task");
        idle(&mut app).await;
        assert_eq!(
            app.team_runtime
                .read(&Participant::Lead, None)
                .await
                .unwrap()
                .entries
                .len(),
            1
        );
        let contexts = mock.contexts.lock().unwrap();
        assert!(
            contexts[0]
                .tools
                .iter()
                .flatten()
                .all(|tool| !["team_post", "team_read"].contains(&tool.name.as_str()))
        );
        assert!(contexts[0].messages.iter().all(|message| !matches!(message, Message::User(user) if matches!(&user.content, UserContentBody::Text(text) if text.contains("attributed finding")))));
    }

    fn review_app(explicit: Option<&str>, ambiguous: bool) -> (App, Arc<Mock>) {
        let (app, mock) = app();
        let mut meta = app.meta;
        meta.settings
            .agents
            .get_mut("custom_42")
            .unwrap()
            .description = Some("Reviews diffs for correctness bugs.".into());
        if ambiguous {
            meta.settings.agents.insert(
                "other-lens".into(),
                meta.settings.agents["custom_42"].clone(),
            );
        }
        if let Some(target) = explicit {
            meta.settings
                .agent_commands
                .insert("review".into(), target.into());
        }
        std::fs::write(
            std::path::Path::new(&meta.cwd).join("sample.rs"),
            "pub fn answer() -> u8 { 42 }\n",
        )
        .unwrap();
        (App::new(app.agent, meta, app.recorder), mock)
    }

    #[tokio::test]
    async fn unique_discovered_review_is_routed_and_records_runtime_evidence() {
        use cupel_coding_agent::roles::Role;
        let (mut app, _) = review_app(None, false);
        submit(&mut app, "/team add custom_42");
        submit(&mut app, "/team roles");
        assert!(!app.crew.is_built("custom_42"));
        submit(&mut app, "/review sample.rs");
        idle(&mut app).await;
        assert_eq!(app.totals.input, 0);
        assert_eq!(app.crew.spend_of("custom_42").calls, 1);
        let snapshots = app.crew.scheduler().snapshots();
        let selected = snapshots
            .iter()
            .find_map(|task| task.selection.as_ref())
            .unwrap();
        assert_eq!(selected.role, Role::Review);
        assert_eq!(selected.agent, "custom_42");
        assert!(
            selected
                .evidence
                .iter()
                .any(|evidence| evidence.contains("Reviews diffs"))
        );
    }

    #[test]
    fn ambiguous_or_invalid_explicit_review_uses_the_lead_not_an_armed_helper() {
        for (explicit, ambiguous) in [(None, true), (Some("missing"), false)] {
            let (mut app, _) = review_app(explicit, ambiguous);
            submit(&mut app, "/team add custom_42 other-lens");
            submit(&mut app, "/agent custom_42");
            submit(&mut app, "/review sample.rs");
            assert!(app.helper.is_none());
            assert!(app.pending_prompt.is_some());
            assert_eq!(app.armed.as_deref(), Some("custom_42"));
            assert!(app.transcript.cells.iter().any(|cell|matches!(cell,Cell::Notice{text}if text.contains(if ambiguous{"ambiguous"}else{"invalid explicit"}))));
        }
    }

    #[tokio::test]
    async fn queued_discovery_is_revalidated_after_membership_changes() {
        let (mut app, _) = review_app(None, false);
        submit(&mut app, "/team add custom_42");
        submit(&mut app, "/agent custom_42 hang");
        submit(&mut app, "/review sample.rs");
        assert!(app.waiting_helper.as_ref().unwrap().selection.is_some());
        submit(&mut app, "/team clear");
        app.helper.as_ref().unwrap().cancel.cancel();
        idle(&mut app).await;
        assert!(app.helper.is_none());
        assert!(
            app.pending_prompt.is_some(),
            "removed targets fall back to the lead"
        );
    }

    #[tokio::test]
    async fn role_task_selector_keeps_actual_permissions_and_provenance() {
        use cupel_coding_agent::team_conversation::Participant;
        use cupel_coding_agent::team_scheduler::{AccessScope, RunContext, TaskState};
        let (mut app, _) = review_app(None, false);
        submit(&mut app, "/team add custom_42");
        let ask = Arc::clone(
            app.agent
                .tools()
                .iter()
                .find(|tool| tool.name() == "ask")
                .unwrap(),
        );
        app.crew.lead_context().set(RunContext::root(
            Participant::Lead,
            AccessScope::CanWrite,
            "review task".into(),
            tokio_util::sync::CancellationToken::new(),
        ));
        ask.execute(
            "by-role",
            serde_json::json!({"role":"review","task":"check the fixture"}),
            tokio_util::sync::CancellationToken::new(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(app.crew.total_spend().calls, 1);
        let nodes = app.crew.scheduler().snapshots();
        assert!(
            nodes
                .iter()
                .any(|node| node.selection.is_some() && node.access == AccessScope::ReadOnly)
        );
        let run = app.crew.lead_context().current().unwrap();
        let root = run.scope(&app.crew.scheduler()).await.unwrap();
        app.crew
            .scheduler()
            .finish(
                &root,
                TaskState::Succeeded,
                "done",
                cupel_coding_agent::crew::Changes::ReadOnly,
            )
            .await;
        app.crew.lead_context().clear();
    }
}
