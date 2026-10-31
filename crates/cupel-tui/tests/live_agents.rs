//! Opt in, billable M8 verification against Fireworks. Never runs in normal CI.
//! Run with CUPEL_LIVE_AGENTS=1 cargo test p cupel tui test live_agents  ignored nocapture.

#[cfg(test)]
mod tests {
    use std::io::Write as _;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use cupel_agent::{Agent, AgentOptions, ToolExecutionMode};
    use cupel_coding_agent::bootstrap::{self, Frontend};
    use cupel_coding_agent::modes::SessionMeta;
    use cupel_coding_agent::session::SessionRecorder;
    use cupel_core::event_stream::{AssistantMessageStream, assistant_message_channel};
    use cupel_core::provider::Provider;
    use cupel_core::types::{AssistantMessageEvent, Context, Model, StreamOptions};
    use cupel_tui::app::{App, ReloadTarget, Status};
    use cupel_tui::startup::TeamInitialization;
    use cupel_tui::transcript::Cell;
    use futures_util::StreamExt as _;
    use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

    struct Workspace(PathBuf);

    /// Opt in diagnostics for this synthetic fixture only. Never records credentials.
    struct Trace {
        file: Mutex<std::fs::File>,
        next: AtomicUsize,
        secret: String,
    }

    impl Trace {
        fn record(&self, value: &serde_json::Value) {
            let text = value.to_string().replace(&self.secret, "[redacted]");
            writeln!(self.file.lock().unwrap(), "{text}").unwrap();
        }
    }

    struct TracedProvider {
        inner: Arc<dyn Provider>,
        trace: Arc<Trace>,
    }

    impl Provider for TracedProvider {
        fn api(&self) -> &str {
            self.inner.api()
        }
        fn compat_problems(&self, model: &Model) -> Vec<String> {
            self.inner.compat_problems(model)
        }
        fn stream(
            &self,
            model: &Model,
            context: Context,
            mut options: StreamOptions,
        ) -> AssistantMessageStream {
            options.temperature = Some(0.0);
            let id = self.trace.next.fetch_add(1, Ordering::Relaxed);
            self.trace.record(&serde_json::json!({
                "event":"request", "id":id, "model":model, "context":context,
                "temperature":options.temperature, "max_tokens":options.max_tokens,
                "reasoning":options.reasoning, "session":options.session_id,
            }));
            let mut source = self.inner.stream(model, context, options);
            let (stream, sink) = assistant_message_channel();
            let trace = Arc::clone(&self.trace);
            tokio::spawn(async move {
                while let Some(event) = source.next().await {
                    match &event {
                        AssistantMessageEvent::Done { message, .. } => trace.record(
                            &serde_json::json!({"event":"response","id":id,"message":message}),
                        ),
                        AssistantMessageEvent::Error { error, .. } => trace.record(
                            &serde_json::json!({"event":"response","id":id,"message":error}),
                        ),
                        _ => {}
                    }
                    if !sink.emit(event) {
                        break;
                    }
                }
            });
            stream
        }
    }

    impl Drop for Workspace {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn git(cwd: &Path, args: &[&str]) {
        let result = std::process::Command::new("git")
            .current_dir(cwd)
            .args(args)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "git failed: {}",
            String::from_utf8_lossy(&result.stderr)
        );
    }

    fn submit(app: &mut App, text: &str) {
        app.input.insert_str(text);
        app.on_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
    }

    fn esc(app: &mut App) {
        app.on_terminal_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
    }

    async fn idle(app: &mut App) {
        if let Some(prompt) = app.pending_prompt.take() {
            app.recorder.before_prompt(&prompt).await;
            app.start_run(&prompt);
        }
        let finished = tokio::time::timeout(Duration::from_secs(120), async {
            while app.is_running() {
                let event = app.next_event().await;
                app.on_event(event).await;
            }
        })
        .await;
        assert!(
            finished.is_ok(),
            "live run did not finish:\n{}\n{}",
            transcript(app),
            app.crew.scheduler().describe()
        );
        assert_ne!(app.last_answer, Status::Failed, "{}", transcript(app));
    }

    fn transcript(app: &App) -> String {
        app.transcript
            .cells
            .iter()
            .filter_map(|cell| match cell {
                Cell::Agent { text, .. }
                | Cell::Assistant { text }
                | Cell::Answer { text }
                | Cell::Error { text }
                | Cell::Notice { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[tokio::test]
    #[ignore = "billable Fireworks calls; requires explicit CUPEL_LIVE_AGENTS=1 opt-in"]
    async fn fireworks_multi_agent_baseline() {
        assert_eq!(
            std::env::var("CUPEL_LIVE_AGENTS").as_deref(),
            Ok("1"),
            "explicit live-test opt-in required"
        );
        let source = cupel_coding_agent::resources::config_home().expect("Cupel home required");
        let source =
            cupel_coding_agent::settings::load_settings(&source.join("settings.json")).unwrap();
        let key = source
            .providers
            .get("fireworks")
            .expect("Fireworks key required");
        let root = std::env::var_os("CUPEL_LIVE_AGENTS_ROOT").map_or_else(
            || {
                std::env::temp_dir().join(format!(
                    "cupel-live-m8-{}-{}",
                    std::process::id(),
                    cupel_core::types::now_ms()
                ))
            },
            PathBuf::from,
        );
        assert!(
            !root.exists(),
            "live fixture root must be a fresh, disposable directory"
        );
        std::fs::create_dir_all(root.join("project/src")).unwrap();
        let root = Workspace(std::fs::canonicalize(root).unwrap());
        let cwd = root.0.join("project");
        let home = root.0.join("home");
        std::fs::create_dir_all(&home).unwrap();
        let mut settings = serde_json::json!({
            "checkForUpdates":false,
            "providers":{"fireworks":key},
            "agents":{
                "map-reader":{"provider":"fireworks","model":"accounts/fireworks/models/minimax-m3p1-flash","thinkingLevel":"off","description":"Finds file definitions and transcript paths. Read-only.","remember":false,"maxTurns":4},
                "diff-auditor":{"provider":"fireworks","model":"accounts/fireworks/models/minimax-m3","thinkingLevel":"low","description":"Reviews diffs and performs small edits or shell tasks explicitly requested by the user.","tools":["read","grep","bash"],"maxTurns":20}
            },
            "agentCommands":{"review":"diff-auditor"}
        });
        let matched = std::env::var("CUPEL_LIVE_MATCHED_TASK").as_deref() == Ok("1");
        if matched {
            settings["agents"]["change-lens"] = serde_json::json!({
                "provider":"fireworks","model":"accounts/fireworks/models/minimax-m3p1-flash",
                "thinkingLevel":"off","description":"Reviews source changes and reports correctness findings.",
                "maxTurns":4,"remember":false
            });
            settings["agents"]["diff-auditor"]["tools"] =
                serde_json::json!(["read", "grep", "apply_patch"]);
        }
        let settings_path = home.join("settings.json");
        {
            use std::io::Write as _;
            use std::os::unix::fs::OpenOptionsExt as _;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&settings_path)
                .unwrap();
            file.write_all(settings.to_string().as_bytes()).unwrap();
        }
        std::fs::write(
            cwd.join("src/lib.rs"),
            "pub const TRANSCRIPT: &str = \"LEDGER42.jsonl\";\n",
        )
        .unwrap();
        git(&cwd, &["init", "-q"]);
        git(&cwd, &["add", "src/lib.rs"]);
        git(
            &cwd,
            &[
                "-c",
                "user.name=Live Test",
                "-c",
                "user.email=live@example.invalid",
                "commit",
                "-qm",
                "fixture",
            ],
        );
        cupel_coding_agent::project_trust::save(
            &home,
            &cwd,
            cupel_coding_agent::project_trust::ProjectTrust::Trusted,
        )
        .unwrap();
        let mut registry = cupel_core::default_registry();
        if let Some(path) = std::env::var_os("CUPEL_LIVE_AGENTS_TRACE") {
            use std::os::unix::fs::OpenOptionsExt as _;
            let trace = Arc::new(Trace {
                file: Mutex::new(
                    std::fs::OpenOptions::new()
                        .create_new(true)
                        .write(true)
                        .mode(0o600)
                        .open(path)
                        .unwrap(),
                ),
                next: AtomicUsize::new(0),
                secret: key.clone(),
            });
            for api in ["openai-completions", "anthropic-messages"] {
                let inner = registry.get(api).unwrap();
                registry.register(Arc::new(TracedProvider {
                    inner,
                    trace: Arc::clone(&trace),
                }));
            }
        }
        let registry = Arc::new(registry);
        let mut ingredients =
            bootstrap::load(&cwd, Some(home.clone()), &registry, Frontend::Tui).await;
        // Bound each request, independently of provider catalog output limits.
        for model in &mut ingredients.models {
            // MiniMax's low thinking budget is 1024, max_tokens must exceed it.
            model.max_tokens = model.max_tokens.min(4096);
        }
        let bounded_models = ingredients
            .models
            .iter()
            .filter(|model| model.provider.as_str() == "fireworks")
            .cloned()
            .collect::<Vec<_>>();
        std::fs::write(
            home.join("models.json"),
            serde_json::to_vec(&bounded_models).unwrap(),
        )
        .unwrap();
        let model = ingredients
            .models
            .iter()
            .find(|model| model.id == "accounts/fireworks/models/minimax-m3p1-flash")
            .unwrap()
            .clone();
        let mut options = AgentOptions::new(model.clone(), registry);
        options.api_key = Some(key.clone());
        options.system_prompt = ingredients.system_prompt.clone();
        options.tools = ingredients.tools;
        options.hooks = Arc::new(ingredients.hooks);
        options.tool_execution = ToolExecutionMode::Parallel;
        options.thinking_level = None;
        options.max_tokens = Some(4096);
        let meta = SessionMeta {
            model_name: model.name,
            provider: "fireworks".into(),
            cwd: cwd.to_string_lossy().into_owned(),
            templates: ingredients.templates,
            models: ingredients.models,
            settings: ingredients.settings,
            home: Some(home.clone()),
            startup_warning: None,
            warnings: ingredients.warnings,
            context_files: ingredients.context_files,
            base_system_prompt: ingredients.system_prompt,
        };
        let mut app = App::new(
            Agent::new(options),
            meta,
            SessionRecorder::new(Some(home.clone()), &cwd, "live-m8", &model.id),
        );
        app.questions = ingredients.questions;
        app.initialize_team(TeamInitialization::ConfiguredDefaults);
        if matched {
            matched_workflow(&mut app, &cwd, &home).await;
            return;
        }
        if std::env::var("CUPEL_LIVE_EXTENDED_ONLY").as_deref() == Ok("1") {
            submit(&mut app, "/team add map-reader diff-auditor");
            expanded(&mut app, &settings_path, &settings, &cwd).await;
            let totals = app.session_totals();
            println!(
                "Expanded Fireworks usage: {} input, {} output, {} cached, ${:.6}",
                totals.input, totals.output, totals.cache_read, totals.cost
            );
            return;
        }
        assert!(app.meta.warnings.is_empty());
        submit(&mut app, "/team");
        assert!(!app.crew.is_built("map-reader"));
        assert!(!app.crew.is_built("diff-auditor"));
        println!("PASS lazy startup roster");

        let mut typo = settings.clone();
        typo["agents"]["map-reader"]["model"] = "not-a-real-model".into();
        std::fs::write(&settings_path, typo.to_string()).unwrap();
        app = app.hot_reload(ReloadTarget::Current).await;
        assert!(transcript(&app).contains("ignoring agent map-reader: unknown model"));
        std::fs::write(&settings_path, settings.to_string()).unwrap();
        app = app.hot_reload(ReloadTarget::Current).await;
        assert!(app.crew.spec("map-reader").is_some());
        println!("PASS typo and hot reload");

        submit(
            &mut app,
            "/agent map-reader Read src/lib.rs and report the exact TRANSCRIPT value. Do not guess.",
        );
        idle(&mut app).await;
        assert!(transcript(&app).contains("LEDGER42"));
        assert_eq!(app.crew.spend_of("map-reader").calls, 1);
        assert_eq!(app.totals.input, 0);
        submit(
            &mut app,
            "What exact transcript filename did map-reader just find? Answer from the exchange, without tools.",
        );
        idle(&mut app).await;
        assert!(
            app.transcript
                .cells
                .iter()
                .rev()
                .any(|cell| matches!(cell, Cell::Answer { text } if text.contains("LEDGER42")))
        );
        println!("PASS direct helper and lead exchange awareness");

        submit(&mut app, "/agent map-reader");
        assert_eq!(app.armed.as_deref(), Some("map-reader"));
        esc(&mut app);
        assert!(app.armed.is_none());
        submit(&mut app, "/agent map-reader");
        submit(
            &mut app,
            "Read src/lib.rs and give the constant's value only.",
        );
        idle(&mut app).await;
        assert!(app.armed.is_none());
        assert_eq!(app.crew.spend_of("map-reader").calls, 2);
        println!("PASS one-shot arming and Esc");

        submit(&mut app, "/team add map-reader diff-auditor");
        assert_eq!(app.team.len(), 2);
        submit(
            &mut app,
            "Use your ask tool to ask map-reader to read src/lib.rs and report the TRANSCRIPT value. You must call ask, not do the reading yourself. Then report its answer.",
        );
        idle(&mut app).await;
        assert!(
            app.crew.spend_of("map-reader").calls >= 3,
            "lead did not delegate: {}",
            transcript(&app)
        );
        println!("PASS model-initiated ask");

        submit(
            &mut app,
            "Use delegate to ask diff-auditor to create edited.txt containing exactly verified using bash. This is a disposable test checkout; do not edit any other files. Report its changed-file result.",
        );
        idle(&mut app).await;
        assert_eq!(
            std::fs::read_to_string(cwd.join("edited.txt"))
                .unwrap()
                .trim(),
            "verified"
        );
        assert!(app.crew.spend_of("diff-auditor").calls >= 1);
        println!("PASS model-initiated writer and observed edits");

        submit(
            &mut app,
            "Use delegate to ask diff-auditor to run exactly: echo started > waiting.txt; sleep 90. This cancellation test needs that bash command, not a simulated sleep. Do nothing else until its tool returns. If it is stopped, the cancellation is intentional: never retry, never run its command yourself, and respond only 'cancellation acknowledged'.",
        );
        if let Some(prompt) = app.pending_prompt.take() {
            app.recorder.before_prompt(&prompt).await;
            app.start_run(&prompt);
        }
        tokio::time::timeout(Duration::from_secs(120), async {
            while !cwd.join("waiting.txt").exists() {
                tokio::select! {
                    event=app.next_event()=>app.on_event(event).await,
                    ()=tokio::time::sleep(Duration::from_millis(20))=>{},
                }
            }
        })
        .await
        .expect("lead/helper did not start cancellation fixture");
        esc(&mut app);
        assert!(
            app.run_events.is_some(),
            "first Esc must leave the lead running"
        );
        idle(&mut app).await;
        println!("PASS Esc stops delegated work while lead continues");

        std::fs::write(
            cwd.join("src/lib.rs"),
            "pub const TRANSCRIPT: &str = \"LEDGER43.jsonl\";\n",
        )
        .unwrap();
        let calls = app.crew.spend_of("diff-auditor").calls;
        submit(&mut app, "/review --diff");
        idle(&mut app).await;
        assert_eq!(app.crew.spend_of("diff-auditor").calls, calls + 1);
        println!("PASS explicit review routing");

        let spend = app.crew.total_spend();
        let path = cupel_coding_agent::session::sessions_dir(Some(&home), &cwd)
            .unwrap()
            .join("live-m8.jsonl");
        let (_, messages) = cupel_coding_agent::session::load_transcript(&path).unwrap();
        let mut options = AgentOptions::new(
            app.meta
                .models
                .iter()
                .find(|m| m.id == model.id)
                .unwrap()
                .clone(),
            app.agent.registry(),
        );
        options.messages = messages;
        options.api_key = Some(key.clone());
        options.system_prompt = app.meta.base_system_prompt.clone();
        options.tools = bootstrap::coding_tools(&cwd);
        options.tool_execution = ToolExecutionMode::Parallel;
        options.max_tokens = Some(4096);
        let mut resumed = App::new(
            Agent::new(options),
            app.meta,
            SessionRecorder::new(Some(home.clone()), &cwd, "live-m8", &model.id),
        );
        resumed.initialize_team(TeamInitialization::Restore);
        assert_eq!(resumed.team.len(), 2);
        assert!(!resumed.crew.is_built("diff-auditor"));
        assert_eq!(resumed.crew.total_spend(), spend);
        assert!(path.with_extension("").join("map-reader.jsonl").exists());
        assert!(path.with_extension("").join("diff-auditor.jsonl").exists());
        submit(
            &mut resumed,
            "/agent diff-auditor What file did you create earlier in this session? Answer from memory without tools.",
        );
        idle(&mut resumed).await;
        assert!(transcript(&resumed).contains("edited.txt"));
        let total = resumed.session_totals();
        assert!(total.input > resumed.totals.input);
        println!("PASS lazy resume, helper memory, team and spend restoration");
        expanded(&mut resumed, &settings_path, &settings, &cwd).await;
        let total = resumed.session_totals();
        let helpers = resumed.crew.total_spend();
        println!(
            "Fireworks helpers: {} calls, {} input, {} output, {} cached, ${:.6}",
            helpers.calls, helpers.input, helpers.output, helpers.cache_read, helpers.cost
        );
        println!(
            "Fireworks combined: {} input, {} output, {} cached, ${:.6}",
            total.input, total.output, total.cache_read, total.cost
        );
        let fresh = resumed.hot_reload(ReloadTarget::New).await;
        assert!(
            fresh
                .team_runtime
                .inspect(None)
                .await
                .unwrap()
                .entries
                .is_empty()
        );
        assert!(fresh.agent.state().messages.is_empty());
        let directory = fresh.recorder.sessions_dir().unwrap();
        assert!(
            !directory
                .join(format!("{}.jsonl", fresh.recorder.session_id()))
                .exists()
        );
        assert!(!directory.join(fresh.recorder.session_id()).exists());
        println!("PASS fresh-session discussion isolation and trace-free listing");
    }

    async fn matched_workflow(app: &mut App, cwd: &Path, home: &Path) {
        use cupel_coding_agent::team_conversation::Participant;
        use cupel_coding_agent::team_scheduler::TaskState;
        use cupel_core::types::{Message, ToolResultContent};
        let original = std::fs::read(cwd.join("src/lib.rs")).unwrap();
        submit(app, "/team add map-reader diff-auditor change-lens");
        app.team_runtime.post(Participant::Lead,
            "User constraint for this task: preserve src/lib.rs byte-for-byte. The required result marker is SHARED_19; carry it through the final answer and result.json.".into(), None).await.unwrap();
        submit(
            app,
            "/agent diff-auditor Use ask to assign change-lens this review: read src/lib.rs and return a JSON object with status REVIEW_OK and transcript equal to the TRANSCRIPT string literal's contents (exclude the Rust declaration and quotation marks); do not change files. After that child succeeds, create result.json with exactly two fields: transcript (copy the child's transcript field) and marker (the required marker from shared discussion). Preserve src/lib.rs byte-for-byte. Finish with a brief answer containing the value and marker. Do not substitute your own review for the child.",
        );
        idle(app).await;
        let nodes = app.crew.scheduler().snapshots();
        assert!(
            nodes.iter().any(|task| task.parent.is_some()
                && task.actor == Participant::Helper("change-lens".into())
                && task.state == TaskState::Succeeded
                && task.summary.contains("REVIEW_OK")
                && task.summary.contains("LEDGER42.jsonl")),
            "child must finish the actual review with the observed value"
        );
        println!("PASS child review result");
        let path = cupel_coding_agent::session::sessions_dir(Some(home), cwd)
            .unwrap()
            .join("live-m8/change-lens.jsonl");
        let (_, history) = cupel_coding_agent::session::load_transcript(&path).unwrap();
        assert!(history.iter().any(|message| matches!(message,
            cupel_agent::AgentMessage::Llm(Message::ToolResult(result)) if result.tool_name == "read"
                && result.content.iter().any(|part| matches!(part, ToolResultContent::Text(text) if text.text.contains("LEDGER42.jsonl"))))),
            "reviewer must observe real file contents, not just repeat task text");
        println!("PASS reviewer read real source evidence");
        let result: serde_json::Value =
            serde_json::from_slice(&std::fs::read(cwd.join("result.json")).unwrap()).unwrap();
        assert_eq!(
            result,
            serde_json::json!({"transcript":"LEDGER42.jsonl","marker":"SHARED_19"})
        );
        assert_eq!(std::fs::read(cwd.join("src/lib.rs")).unwrap(), original);
        assert!(
            nodes.iter().all(|task| task.state == TaskState::Succeeded),
            "no cancelled, failed or exhausted work may count as success"
        );
        println!("PASS exact artifact and unchanged source");
        submit(
            app,
            "What exact transcript value and required marker did the team verify? Answer from shared findings without tools, using both exact strings.",
        );
        idle(app).await;
        let answer = app
            .transcript
            .cells
            .iter()
            .rev()
            .find_map(|cell| match cell {
                Cell::Answer { text } => Some(text),
                _ => None,
            })
            .unwrap();
        assert!(answer.contains("LEDGER42.jsonl") && answer.contains("SHARED_19"));
        println!("PASS shared knowledge reaches lead");
        let total = app.session_totals();
        println!(
            "Matched combined: {} input, {} output, {} cached, ${:.6}",
            total.input, total.output, total.cache_read, total.cost
        );
    }

    async fn reload(app: &mut App, settings_path: &Path, settings: &serde_json::Value) {
        std::fs::write(settings_path, settings.to_string()).unwrap();
        assert!(!app.is_running());
        // App consumes itself on reload, use an independently constructed placeholder
        // only to satisfy ownership. It has no credentials, history or running work.
        let model = app.meta.models[0].clone();
        let placeholder = App::new(
            Agent::new(AgentOptions::new(model.clone(), app.agent.registry())),
            SessionMeta {
                model_name: model.name,
                provider: model.provider.as_str().into(),
                cwd: app.meta.cwd.clone(),
                templates: Vec::new(),
                models: Vec::new(),
                settings: cupel_coding_agent::settings::Settings::default(),
                home: None,
                startup_warning: None,
                warnings: Vec::new(),
                context_files: Vec::new(),
                base_system_prompt: String::new(),
            },
            SessionRecorder::new(None, Path::new(&app.meta.cwd), "unused", "unused"),
        );
        let old = std::mem::replace(app, placeholder);
        *app = old.hot_reload(ReloadTarget::Current).await;
    }

    async fn expanded(
        app: &mut App,
        settings_path: &Path,
        original: &serde_json::Value,
        cwd: &Path,
    ) {
        use cupel_coding_agent::roles::Role;
        use cupel_coding_agent::team_conversation::{Participant, TeamBody};
        use cupel_coding_agent::team_scheduler::{AccessScope, RunContext, TaskState};
        use cupel_tui::app::TaskAction;
        let mut settings = original.clone();
        settings.as_object_mut().unwrap().remove("agentCommands");
        // Baseline already verifies remembered history. Extension scenarios use a
        // fresh private writer history so old cancellation instructions do not
        // steer a new, unrelated live test task.
        settings["agents"]["diff-auditor"]["remember"] = false.into();
        reload(app, settings_path, &settings).await;
        submit(app, "/team roles");
        assert!(!app.crew.is_built("diff-auditor"));
        assert!(!app.crew.is_built("map-reader"));
        assert!(transcript(app).contains("review: diff-auditor"));
        submit(app, "/review src/lib.rs");
        idle(app).await;
        assert!(app.crew.scheduler().snapshots().iter().any(|task| {
            task.selection.as_ref().is_some_and(|selection| {
                selection.role == Role::Review && selection.agent == "diff-auditor"
            })
        }));
        let calls = app.crew.spend_of("diff-auditor").calls;
        app.apply_startup(cupel_tui::startup::StartupCommand::Review);
        idle(app).await;
        assert_eq!(app.crew.spend_of("diff-auditor").calls, calls + 1);
        println!("PASS discovered /review and typed startup --review with no explicit mapping");

        settings["agents"]["change-lens"] = serde_json::json!({"provider":"fireworks","model":"accounts/fireworks/models/minimax-m3p1-flash","thinkingLevel":"off","description":"Reviews source changes and reports correctness findings.","prompt":"Live verification fixture: after reading the requested source file, always publish exactly one team_post with text REVIEW_OK before returning your answer. Never edit files.","maxTurns":4,"remember":false});
        settings["agents"]["shell-worker"] = serde_json::json!({"provider":"fireworks","model":"accounts/fireworks/models/minimax-m3p1-flash","thinkingLevel":"off","description":"Implements explicitly requested shell tasks in the disposable checkout.","tools":["bash"],"maxTurns":3,"remember":false});
        reload(app, settings_path, &settings).await;
        submit(app, "/team add change-lens shell-worker");
        submit(
            app,
            "/agent diff-auditor This is an end-to-end teamwork test. First use team_post to post the question 'Please review src/lib.rs'. Then use ask with role=review to assign this exact task to another teammate: 'Read src/lib.rs. Use team_post to post REVIEW_OK with a concise finding, then return REVIEW_OK. Do not edit files.' Finally integrate the child's answer and post your conclusion using team_post. Do not substitute your own review for the child call.",
        );
        idle(app).await;
        let nodes = app.crew.scheduler().snapshots();
        assert!(
            nodes.iter().any(|task| task.parent.is_some()
                && task.actor == Participant::Helper("change-lens".into())
                && task
                    .selection
                    .as_ref()
                    .is_some_and(|selection| selection.role == Role::Review)),
            "writer did not make the required nested role call"
        );
        assert!(
            nodes.iter().any(|task| task.parent.is_some()
                && task.actor == Participant::Helper("change-lens".into())
                && task.state == TaskState::Succeeded
                && task.summary.contains("REVIEW_OK")),
            "the actual child reviewer must succeed and return REVIEW_OK, not merely mention it in an error"
        );
        let page = app.team_runtime.inspect(None).await.unwrap();
        for name in ["diff-auditor", "change-lens"] {
            assert!(
                page.entries
                    .iter()
                    .any(|entry| entry.author == Participant::Helper(name.into())
                        && matches!(&entry.body, TeamBody::Post { .. })),
                "missing attributed post for {name}: {}\n{}",
                transcript(app),
                app.crew.scheduler().describe()
            );
        }
        assert!(transcript(app).contains("REVIEW_OK"));
        submit(
            app,
            "What exact review marker did the team just post? Answer from the shared conversation, without tools.",
        );
        idle(app).await;
        assert!(
            app.transcript
                .cells
                .iter()
                .rev()
                .any(|cell| matches!(cell,Cell::Answer{text}if text.contains("REVIEW_OK"))),
            "{}",
            transcript(app)
        );
        println!(
            "PASS nested role-based review, attributed discussion and integrated lead awareness"
        );

        // Force same specialist concurrency directly through the real task tools,
        // rather than depending on a model deciding to emit parallel tool calls.
        app.crew.lead_context().set(RunContext::root(
            Participant::Lead,
            AccessScope::CanWrite,
            "parallel specialist verification".into(),
            tokio_util::sync::CancellationToken::new(),
        ));
        let ask = Arc::clone(
            app.agent
                .tools()
                .iter()
                .find(|tool| tool.name() == "ask")
                .unwrap(),
        );
        let mut jobs = tokio::task::JoinSet::new();
        for _ in 0..2 {
            let ask = Arc::clone(&ask);
            jobs.spawn(async move{ask.execute("live-parallel",serde_json::json!({"agent":"change-lens","task":"Read src/lib.rs and reply with REVIEW_OK only."}),tokio_util::sync::CancellationToken::new(),None).await.unwrap()});
        }
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let tasks = app.crew.scheduler().snapshots();
                let same = tasks
                    .iter()
                    .filter(|task| task.actor == Participant::Helper("change-lens".into()))
                    .collect::<Vec<_>>();
                if same.iter().any(|task| task.state == TaskState::Queued)
                    && same.iter().any(|task| task.state == TaskState::Running)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("same specialist was not observably queued");
        while let Some(result) = jobs.join_next().await {
            result.unwrap();
        }
        let run = app.crew.lead_context().current().unwrap();
        let root = run.scope(&app.crew.scheduler()).await.unwrap();
        app.crew
            .scheduler()
            .finish(
                &root,
                TaskState::Succeeded,
                "parallel checks complete",
                cupel_coding_agent::crew::Changes::ReadOnly,
            )
            .await;
        app.crew.lead_context().clear();
        println!("PASS concurrent real-provider branches queue for one busy specialist");

        submit(
            app,
            "/agent diff-auditor Use delegate to assign shell-worker this exact bash command: echo $$ > nested.pid; echo started > nested-started.txt; sleep 90; echo late > must-not-exist.txt . This is a cancellation test. You must call delegate and wait. If the child is stopped, never retry or run its command yourself; report the stop.",
        );
        tokio::time::timeout(Duration::from_secs(120), async {
            while !cwd.join("nested-started.txt").exists() {
                assert!(
                    app.is_running(),
                    "nested fixture ended without starting its writer:\n{}\n{}",
                    transcript(app),
                    app.crew.scheduler().describe()
                );
                tokio::select! {
                    event=app.next_event()=>app.on_event(event).await,
                    ()=tokio::time::sleep(Duration::from_millis(20))=>{},
                }
            }
        })
        .await
        .expect("nested writer did not start");
        let tasks = app.crew.scheduler().snapshots();
        let child = tasks
            .iter()
            .find(|task| {
                task.actor == Participant::Helper("shell-worker".into())
                    && task.state == TaskState::Running
            })
            .expect("missing running nested task");
        assert!(
            !cwd.join("must-not-exist.txt").exists(),
            "the sleeper must still be running before cancellation"
        );
        let cancelling = std::time::Instant::now();
        app.apply_task_action(TaskAction::Cancel(child.id.0)).await;
        esc(app);
        idle(app).await;
        assert!(
            cancelling.elapsed() < Duration::from_secs(15),
            "cancellation must not wait for the 90-second sleep"
        );
        app.apply_task_action(TaskAction::Inspect(Some(child.root.0)))
            .await;
        assert!(!cwd.join("must-not-exist.txt").exists());
        assert!(transcript(app).contains("nested-started.txt"));
        let pid = std::fs::read_to_string(cwd.join("nested.pid"))
            .unwrap()
            .trim()
            .parse::<u32>()
            .unwrap();
        assert!(pid > 1);
        let alive = std::process::Command::new("kill")
            .args(["-0", &format!("-{pid}")])
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(
            !alive.success(),
            "nested shell process group survived cancellation"
        );
        assert!(
            app.crew
                .scheduler()
                .snapshots()
                .iter()
                .all(|task| task.state.terminal())
        );
        println!(
            "PASS task-ID subtree cancellation, Esc, process cleanup and partial change reporting"
        );

        submit(app, "/review src/lib.rs");
        assert!(app.helper.is_none());
        assert!(app.pending_prompt.take().is_some());
        assert!(transcript(app).contains("ambiguous review"));
        settings["agentCommands"] = serde_json::json!({"review":"diff-auditor"});
        reload(app, settings_path, &settings).await;
        settings["agents"]
            .as_object_mut()
            .unwrap()
            .remove("diff-auditor");
        reload(app, settings_path, &settings).await;
        submit(app, "/review src/lib.rs");
        assert!(app.helper.is_none());
        assert!(app.pending_prompt.take().is_some());
        assert!(transcript(app).contains("invalid explicit"));
        let spent = app.crew.total_spend();
        let entries = app.team_runtime.inspect(None).await.unwrap().latest;
        reload(app, settings_path, &settings).await;
        app.crew.recover_usage().await.unwrap();
        assert_eq!(app.crew.total_spend(), spent);
        assert_eq!(
            app.team_runtime.inspect(None).await.unwrap().latest,
            entries
        );
        assert!(!app.team.contains("diff-auditor"));
        assert!(
            app.crew
                .scheduler()
                .snapshots()
                .iter()
                .all(|task| task.state.terminal())
        );
        println!(
            "PASS ambiguity, invalid explicit precedence, removed-member reload and spend/thread retention"
        );
    }
}
