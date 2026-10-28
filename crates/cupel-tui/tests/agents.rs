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
}
