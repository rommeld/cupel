//! Configured Developer/Reviewer handoffs through the real TUI, crew, and tools.

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use cupel_agent::{Agent, AgentOptions};
    use cupel_coding_agent::bootstrap;
    use cupel_coding_agent::crew::Changes;
    use cupel_coding_agent::modes::SessionMeta;
    use cupel_coding_agent::resources::ContextFile;
    use cupel_coding_agent::roles::{AccessFilter, Role};
    use cupel_coding_agent::session::SessionRecorder;
    use cupel_coding_agent::settings::Settings;
    use cupel_coding_agent::team_conversation::Participant;
    use cupel_coding_agent::team_scheduler::{AccessScope, RunContext, TaskState};
    use cupel_core::event_stream::{AssistantMessageStream, assistant_message_channel};
    use cupel_core::provider::{Provider, Registry};
    use cupel_core::types::{
        Api, AssistantContent, AssistantMessage, Context, Message, Model, StopReason,
        StreamOptions, TextContent, ToolCall, ToolResultContent, Usage, UserContentBody,
    };
    use cupel_tui::app::{App, Status};
    use cupel_tui::startup::TeamInitialization;
    use cupel_tui::transcript::Cell;
    use futures_util::StreamExt as _;
    use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use tokio_util::sync::CancellationToken;

    const CONFIG: &str = include_str!("../../../.cupel/settings.json");
    const REPORT: &str = ".cupel/reviews/cap.md";
    const ORIGINAL: &str = "pub fn cap(value: u8, limit: u8) -> u8 {\n    value\n}\n";
    const BROKEN: &str = "pub fn cap(value: u8, limit: u8) -> u8 {\n    value.max(limit)\n}\n";
    const FIXED: &str = "pub fn cap(value: u8, limit: u8) -> u8 {\n    value.min(limit)\n}\n";
    const GUIDE: &str = "# Cap implementation\n\nChange src/lib.rs::cap to return the smaller of value and limit. Preserve the public signature. Tests in tests/cap.rs cover above, below, and equal values. Run formatting, Clippy, tests, and compile checks.\n";

    struct Fixture(PathBuf);

    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir().join(format!(
                "cupel-developer-reviewer-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            std::fs::create_dir_all(root.join("project/src")).unwrap();
            std::fs::create_dir_all(root.join("project/tests")).unwrap();
            std::fs::create_dir_all(root.join("home")).unwrap();
            let fixture = Self(root.canonicalize().unwrap());
            let cwd = fixture.cwd();
            std::fs::write(cwd.join("src/lib.rs"), ORIGINAL).unwrap();
            std::fs::write(cwd.join("private.txt"), "PRIVATE_DEVELOPER_MARKER\n").unwrap();
            std::fs::write(cwd.join("IMPLEMENTATION.md"), GUIDE).unwrap();
            std::fs::write(
                cwd.join("Cargo.toml"),
                "[package]\nname = \"cap-fixture\"\nversion = \"0.0.0\"\nedition = \"2024\"\n",
            )
            .unwrap();
            std::fs::write(
            cwd.join("tests/cap.rs"),
            "#[test]\nfn caps_without_raising_values() {\n    assert_eq!(cap_fixture::cap(9, 4), 4);\n    assert_eq!(cap_fixture::cap(2, 4), 2);\n    assert_eq!(cap_fixture::cap(4, 4), 4);\n}\n",
        )
        .unwrap();
            std::fs::write(cwd.join(".gitignore"), "target/\n.cupel/\n").unwrap();
            std::fs::write(
            cwd.join("AGENTS.md"),
            "Preserve the public cap signature. Only edit the assigned files. Read RUST-CODING-GUIDE.md for Rust changes.\n",
        )
        .unwrap();
            std::fs::write(
                cwd.join("RUST-CODING-GUIDE.md"),
                include_str!("../../../RUST-CODING-GUIDE.md"),
            )
            .unwrap();
            fixture.git(&["init", "-q"]);
            fixture.git(&["add", "."]);
            fixture.git(&[
                "-c",
                "user.name=Workflow Test",
                "-c",
                "user.email=workflow@example.invalid",
                "commit",
                "-qm",
                "baseline",
            ]);
            fixture
        }

        fn cwd(&self) -> PathBuf {
            self.0.join("project")
        }

        fn git(&self, args: &[&str]) {
            let output = std::process::Command::new("git")
                .current_dir(self.cwd())
                .args(args)
                .output()
                .unwrap();
            assert!(output.status.success(), "git: {output:?}");
        }

        fn app(&self, registry: Arc<Registry>, mock: bool) -> App {
            let settings: Settings = serde_json::from_str(CONFIG).unwrap();
            let mut models = cupel_core::catalog::builtin_models();
            if mock {
                for model in &mut models {
                    model.api = Api::from("workflow");
                }
            }
            let model = settings.presets["default"]
                .find_model(&models)
                .unwrap()
                .clone();
            let cwd = self.cwd();
            let context_files = vec![ContextFile {
                path: cwd.join("AGENTS.md"),
                content: std::fs::read_to_string(cwd.join("AGENTS.md")).unwrap(),
            }];
            let base = cupel_coding_agent::system_prompt::build_system_prompt(
                &cwd,
                bootstrap::TOOL_SUMMARIES,
                &context_files,
            );
            let mut options = AgentOptions::new(model.clone(), registry);
            options.system_prompt = base.clone();
            options.tools = bootstrap::coding_tools(&cwd);
            let meta = SessionMeta {
                model_name: model.name,
                provider: model.provider.as_str().into(),
                cwd: cwd.to_string_lossy().into_owned(),
                templates: Vec::new(),
                models,
                settings,
                home: Some(self.0.join("home")),
                startup_warning: None,
                warnings: Vec::new(),
                context_files,
                base_system_prompt: base,
            };
            let mut app = App::new(
                Agent::new(options),
                meta,
                SessionRecorder::new(Some(self.0.join("home")), &cwd, "workflow", &model.id),
            );
            app.initialize_team(TeamInitialization::ConfiguredDefaults);
            app
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    struct Step {
        agent: &'static str,
        content: AssistantContent,
    }

    fn answer(agent: &'static str, text: &str) -> Step {
        Step {
            agent,
            content: AssistantContent::Text(TextContent::plain(text)),
        }
    }

    fn tool(agent: &'static str, name: &str, args: serde_json::Value) -> Step {
        Step {
            agent,
            content: AssistantContent::ToolCall(ToolCall {
                id: format!("{name}-{}", cupel_core::types::now_ms()),
                name: name.into(),
                arguments: args,
            }),
        }
    }

    fn read(agent: &'static str, path: &str) -> Step {
        tool(agent, "read", serde_json::json!({"path": path}))
    }

    fn patch(agent: &'static str, input: &str) -> Step {
        tool(agent, "apply_patch", serde_json::json!({"input": input}))
    }

    struct Script {
        steps: Mutex<VecDeque<Step>>,
        contexts: Mutex<Vec<(String, Context)>>,
    }

    impl Provider for Script {
        fn api(&self) -> &str {
            "workflow"
        }

        fn stream(
            &self,
            model: &Model,
            context: Context,
            _options: StreamOptions,
        ) -> AssistantMessageStream {
            let step = self.steps.lock().unwrap().pop_front().expect("script step");
            assert!(
                context.system_prompt.as_deref().unwrap().contains(&format!(
                    "You are the {}.",
                    if step.agent == "developer" {
                        "Developer"
                    } else {
                        "Reviewer"
                    }
                )),
                "wrong helper selected"
            );
            self.contexts
                .lock()
                .unwrap()
                .push((step.agent.into(), context));
            let reason = if matches!(step.content, AssistantContent::ToolCall(_)) {
                StopReason::ToolUse
            } else {
                StopReason::Stop
            };
            let message = AssistantMessage {
                content: vec![step.content],
                api: model.api.clone(),
                provider: model.provider.clone(),
                model: model.id.clone(),
                response_model: None,
                response_id: None,
                usage: Usage::default(),
                stop_reason: reason,
                error_message: None,
                error_kind: None,
                timestamp: cupel_core::types::now_ms(),
            };
            let (stream, sink) = assistant_message_channel();
            let _ = sink.start();
            let _ = sink.done(reason, message);
            stream
        }
    }

    fn submit(app: &mut App, text: &str) {
        app.input.insert_str(text);
        app.on_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
    }

    async fn idle(app: &mut App) -> String {
        tokio::time::timeout(Duration::from_secs(180), async {
            while app.is_running() {
                let event = app.next_event().await;
                app.on_event(event).await;
            }
        })
        .await
        .expect("workflow timed out");
        assert_ne!(app.last_answer, Status::Failed, "helper failed");
        assert!(app.pending_prompt.is_none(), "unexpected lead fallback");
        app.transcript
            .cells
            .iter()
            .rev()
            .find_map(|cell| match cell {
                Cell::Agent { text, .. } => Some(text.clone()),
                _ => None,
            })
            .unwrap()
    }

    async fn delegate(app: &mut App, role: Role, task: &str, context: &str) -> String {
        let tool = Arc::clone(
            app.agent
                .tools()
                .iter()
                .find(|tool| tool.name() == "delegate")
                .unwrap(),
        );
        app.crew.lead_context().set(RunContext::root(
            Participant::Lead,
            AccessScope::CanWrite,
            task.into(),
            CancellationToken::new(),
        ));
        let result = tool
            .execute(
                "workflow-handoff",
                serde_json::json!({"role": role.as_str(), "task": task, "context": context}),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();
        let run = app.crew.lead_context().current().unwrap();
        let root = run.scope(&app.crew.scheduler()).await.unwrap();
        app.crew
            .scheduler()
            .finish(
                &root,
                TaskState::Succeeded,
                "handoff returned",
                Changes::ReadOnly,
            )
            .await;
        app.crew.lead_context().clear();
        result
            .content
            .iter()
            .filter_map(|content| match content {
                ToolResultContent::Text(text) => Some(text.text.as_str()),
                ToolResultContent::Image(_) => None,
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[tokio::test]
    async fn configured_developer_reviewer_workflow() {
        let fixture = Fixture::new();
        let script = Arc::new(Script {
            contexts: Mutex::new(Vec::new()),
            steps: Mutex::new(VecDeque::from([
                read("developer", "private.txt"),
                answer(
                    "developer",
                    "STATUS: NEEDS_CLARIFICATION\nShould cap raise values below the limit? Recommend no. No implementation completed.",
                ),
                answer(
                    "developer",
                    "STATUS: AWAITING_REVIEW\nReview IMPLEMENTATION.md before implementation. Report: .cupel/reviews/cap.md.",
                ),
                read("reviewer", "IMPLEMENTATION.md"),
                read("reviewer", "src/lib.rs"),
                patch(
                    "reviewer",
                    "*** Begin Patch\n*** Add File: .cupel/reviews/cap.md\n+# Cap review\n+Scope: IMPLEMENTATION.md; baseline HEAD, guide version 1.\n+STATUS: APPROVED\n+Guide preserves the public signature and requires upper/lower/equal checks. No required findings.\n*** End Patch",
                ),
                answer(
                    "reviewer",
                    "STATUS: APPROVED\nGuide reviewed. Report: .cupel/reviews/cap.md.",
                ),
                read("developer", REPORT),
                patch(
                    "developer",
                    "*** Begin Patch\n*** Update File: src/lib.rs\n@@\n pub fn cap(value: u8, limit: u8) -> u8 {\n-    value\n+    value.max(limit)\n }\n*** End Patch",
                ),
                answer(
                    "developer",
                    "STATUS: AWAITING_REVIEW\nScope: src/lib.rs vs HEAD. Implementation needs independent review. Report: .cupel/reviews/cap.md.",
                ),
                read("reviewer", "src/lib.rs"),
                read("reviewer", "tests/cap.rs"),
                patch(
                    "reviewer",
                    "*** Begin Patch\n*** Update File: .cupel/reviews/cap.md\n@@\n+## Code review\n+Scope: src/lib.rs vs HEAD. STATUS: CHANGES_REQUESTED\n+cap-F001 [P1] src/lib.rs:2: max raises values and does not cap them. Use min; all three tests must pass.\n*** End Patch",
                ),
                answer(
                    "reviewer",
                    "STATUS: CHANGES_REQUESTED\nReport: .cupel/reviews/cap.md. Assign cap-F001 to Developer: use min and run the boundary tests.",
                ),
                read("developer", REPORT),
                read("developer", "src/lib.rs"),
                patch(
                    "developer",
                    "*** Begin Patch\n*** Update File: src/lib.rs\n@@\n pub fn cap(value: u8, limit: u8) -> u8 {\n-    value.max(limit)\n+    value.min(limit)\n }\n*** Update File: .cupel/reviews/cap.md\n@@\n+## Developer follow-up\n+cap-F001: corrected max to min in src/lib.rs. Pending validation and re-review.\n*** End Patch",
                ),
                tool(
                    "developer",
                    "bash",
                    serde_json::json!({"command": "cargo fmt --check && cargo clippy --offline -- -D warnings && cargo test --offline && cargo check --offline"}),
                ),
                answer(
                    "developer",
                    "STATUS: AWAITING_REVIEW\ncap-F001 corrected. Formatting, Clippy, tests and compile checks passed. Re-review .cupel/reviews/cap.md and src/lib.rs.",
                ),
                read("reviewer", REPORT),
                read("reviewer", "src/lib.rs"),
                tool(
                    "reviewer",
                    "bash",
                    serde_json::json!({"command": "cargo test --offline"}),
                ),
                patch(
                    "reviewer",
                    "*** Begin Patch\n*** Update File: .cupel/reviews/cap.md\n@@\n+## Re-review\n+cap-F001: RESOLVED; min preserves the signature and passes all boundary cases.\n+STATUS: APPROVED\n+Validation: cargo test --offline passed; no remaining required findings.\n*** End Patch",
                ),
                answer(
                    "reviewer",
                    "STATUS: APPROVED\nReport: .cupel/reviews/cap.md. cap-F001 resolved; independent cargo test passed.",
                ),
                read("developer", REPORT),
                answer(
                    "developer",
                    "STATUS: COMPLETE\nScope: src/lib.rs vs HEAD. cap-F001 resolved and independently approved in .cupel/reviews/cap.md. Formatting, Clippy, tests and compile checks passed.",
                ),
            ])),
        });
        let mut registry = Registry::new();
        registry.register(Arc::clone(&script) as Arc<dyn Provider>);
        let mut app = fixture.app(Arc::new(registry), true);
        assert_eq!(app.crew.members().len(), 2);
        assert!(!app.crew.is_built("developer") && !app.crew.is_built("reviewer"));
        for (role, name) in [(Role::Implement, "developer"), (Role::Review, "reviewer")] {
            assert_eq!(
                app.crew
                    .roles()
                    .resolve(role, &app.crew.members(), AccessFilter::Writer)
                    .selection(role)
                    .unwrap()
                    .agent,
                name
            );
        }

        let answer = delegate(
            &mut app,
            Role::Implement,
            "Clarify the cap policy before editing.",
            "Report path: .cupel/reviews/cap.md",
        )
        .await;
        assert!(answer.contains("STATUS: NEEDS_CLARIFICATION"));
        assert_eq!(
            std::fs::read_to_string(fixture.cwd().join("src/lib.rs")).unwrap(),
            ORIGINAL
        );
        let answer = delegate(
            &mut app,
            Role::Implement,
            "Use IMPLEMENTATION.md; lower values must be preserved. Request guide review first.",
            "Report path: .cupel/reviews/cap.md",
        )
        .await;
        assert!(answer.contains("STATUS: AWAITING_REVIEW"));
        assert_eq!(
            std::fs::read_to_string(fixture.cwd().join("src/lib.rs")).unwrap(),
            ORIGINAL
        );

        submit(&mut app, "/review IMPLEMENTATION.md");
        assert!(idle(&mut app).await.contains("STATUS: APPROVED"));
        assert!(
            std::fs::read_to_string(fixture.cwd().join(REPORT))
                .unwrap()
                .contains("Guide preserves")
        );
        assert_eq!(
            std::fs::read_to_string(fixture.cwd().join("src/lib.rs")).unwrap(),
            ORIGINAL
        );
        let answer = delegate(
            &mut app,
            Role::Implement,
            "Implement the reviewed guide in src/lib.rs and request code review.",
            "Read .cupel/reviews/cap.md for the guide approval; baseline HEAD.",
        )
        .await;
        assert!(answer.contains("STATUS: AWAITING_REVIEW"));
        assert_eq!(
            std::fs::read_to_string(fixture.cwd().join("src/lib.rs")).unwrap(),
            BROKEN
        );
        submit(&mut app, "/review --diff");
        assert!(idle(&mut app).await.contains("STATUS: CHANGES_REQUESTED"));
        assert_eq!(
            std::fs::read_to_string(fixture.cwd().join("src/lib.rs")).unwrap(),
            BROKEN
        );
        let answer = delegate(
            &mut app,
            Role::Implement,
            "Fix cap-F001 in src/lib.rs; append dispositions, validate, and request re-review.",
            "Report: .cupel/reviews/cap.md; preserve original findings and the public signature.",
        )
        .await;
        assert!(answer.contains("STATUS: AWAITING_REVIEW"));
        let answer = delegate(
            &mut app,
            Role::Review,
            "Re-check cap-F001 against current code, run the tests, and append the verdict.",
            "Report: .cupel/reviews/cap.md. Developer changed only max to min; baseline HEAD.",
        )
        .await;
        assert!(answer.contains("STATUS: APPROVED"));
        let answer = delegate(
            &mut app,
            Role::Implement,
            "Read the independent verdict and return the final task status; do not edit further.",
            "Report: .cupel/reviews/cap.md; required finding cap-F001.",
        )
        .await;
        assert!(answer.contains("STATUS: COMPLETE"));

        assert_eq!(
            std::fs::read_to_string(fixture.cwd().join("src/lib.rs")).unwrap(),
            FIXED
        );
        assert_eq!(
            std::fs::read_to_string(fixture.cwd().join("IMPLEMENTATION.md")).unwrap(),
            GUIDE
        );
        let report = std::fs::read_to_string(fixture.cwd().join(REPORT)).unwrap();
        assert!(
            report.contains("cap-F001 [P1]")
                && report.contains("Developer follow-up")
                && report.contains("cap-F001: RESOLVED")
        );
        assert!(script.steps.lock().unwrap().is_empty());
        let contexts = script.contexts.lock().unwrap();
        for (_, context) in contexts.iter() {
            for message in &context.messages {
                if let Message::ToolResult(result) = message {
                    assert!(!result.is_error, "real workflow tool failed: {result:?}");
                }
            }
        }
        let reviewer_contexts: Vec<_> = contexts
            .iter()
            .filter(|(name, _)| name == "reviewer")
            .collect();
        assert!(reviewer_contexts.iter().all(|(_, context)| {
            !serde_json::to_string(context)
                .unwrap()
                .contains("PRIVATE_DEVELOPER_MARKER")
        }));
        let final_review = reviewer_contexts.last().unwrap();
        assert!(!final_review.1.messages.iter().any(|message| matches!(message, Message::User(user) if matches!(&user.content, UserContentBody::Text(text) if text.starts_with("Review the following code")))));
        assert!(contexts.iter().any(|(_, context)| {
            serde_json::to_string(context)
                .unwrap()
                .contains("test result: ok")
        }));
        assert!(
            contexts
                .iter()
                .filter(|(name, _)| name == "developer")
                .skip(2)
                .any(|(_, context)| serde_json::to_string(context)
                    .unwrap()
                    .contains("PRIVATE_DEVELOPER_MARKER"))
        );
        assert!(
            app.crew
                .scheduler()
                .snapshots()
                .iter()
                .filter_map(|task| task.selection.as_ref())
                .any(
                    |selection| selection.agent == "developer" && selection.role == Role::Implement
                )
        );
        assert_eq!(
            app.totals.input, 0,
            "no lead model call is required for explicit handoffs"
        );
    }

    /// Keep credentials in the real home while all agent files/transcripts stay in
    /// the disposable fixture. Tokens are supplied only to the transport, never to
    /// model messages or fixture files. Normal production refresh updates its source.
    struct LiveProvider {
        inner: Arc<dyn Provider>,
        home: PathBuf,
        http: reqwest::Client,
    }

    impl Provider for LiveProvider {
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
            let inner = Arc::clone(&self.inner);
            let model = model.clone();
            let home = self.home.clone();
            let http = self.http.clone();
            let (stream, sink) = assistant_message_channel();
            tokio::spawn(async move {
                options.api_key =
                    cupel_coding_agent::auth::openai_codex_access_token(Some(&home), &http).await;
                let mut source = inner.stream(&model, context, options);
                while let Some(event) = source.next().await {
                    if !sink.emit(event) {
                        break;
                    }
                }
            });
            stream
        }
    }

    fn fixture_checks(fixture: &Fixture) {
        for args in [
            vec!["fmt", "--check"],
            vec!["clippy", "--offline", "--", "-D", "warnings"],
            vec!["test", "--offline"],
            vec!["check", "--offline"],
        ] {
            let output = std::process::Command::new("cargo")
                .current_dir(fixture.cwd())
                .args(&args)
                .output()
                .unwrap();
            assert!(output.status.success(), "cargo {args:?}: {output:?}");
        }
    }

    #[tokio::test]
    #[ignore = "live configured Codex models; requires CUPEL_LIVE_DEVELOPER_REVIEWER=1 and login"]
    async fn live_configured_developer_reviewer_workflow() {
        assert_eq!(
            std::env::var("CUPEL_LIVE_DEVELOPER_REVIEWER").as_deref(),
            Ok("1")
        );
        let source_home = cupel_coding_agent::resources::config_home().unwrap();
        assert!(cupel_coding_agent::auth::has_credential(
            Some(&source_home),
            "openai-codex"
        ));
        let fixture = Fixture::new();
        let mut registry = cupel_core::default_registry();
        let inner = registry.get("openai-codex-responses").unwrap();
        registry.register(Arc::new(LiveProvider {
            inner,
            home: source_home,
            http: reqwest::Client::new(),
        }));
        let mut app = fixture.app(Arc::new(registry), false);
        let original_tests = std::fs::read(fixture.cwd().join("tests/cap.rs")).unwrap();

        let result = delegate(&mut app, Role::Implement,
        "Implement IMPLEMENTATION.md, but first obtain its required independent review. No guide review exists yet. Return the review request before changing source; do not delegate or poll.",
        "Acceptance: cap returns the smaller u8 without changing its public signature or tests. Guide report path: .cupel/reviews/cap.md; baseline HEAD.").await;
        assert!(result.contains("AWAITING_REVIEW"), "{result}");
        assert_eq!(
            std::fs::read_to_string(fixture.cwd().join("src/lib.rs")).unwrap(),
            ORIGINAL
        );
        println!("PASS live Developer requests guide review before edits");

        let result = delegate(&mut app, Role::Review,
        "Review the full IMPLEMENTATION.md against the actual source, tests, project instructions and Rust guide. Save the guide verdict to .cupel/reviews/cap.md. Do not implement changes.",
        "Baseline HEAD. Acceptance: public cap(u8, u8) -> u8 returns the smaller argument; above/below/equal test coverage is required.").await;
        assert!(result.contains("APPROVED"), "{result}");
        assert!(
            std::fs::read_to_string(fixture.cwd().join(REPORT))
                .unwrap()
                .contains("APPROVED")
        );
        assert_eq!(
            std::fs::read_to_string(fixture.cwd().join("src/lib.rs")).unwrap(),
            ORIGINAL
        );
        println!("PASS live independent guide report and source preservation");

        let result = delegate(&mut app, Role::Implement,
        "Implement the approved IMPLEMENTATION.md in src/lib.rs, run required checks, and request independent code review. Preserve the tests and public signature.",
        "The current guide approval is in .cupel/reviews/cap.md. Baseline HEAD. cap must return the smaller argument.").await;
        assert!(result.contains("AWAITING_REVIEW"), "{result}");
        fixture_checks(&fixture);
        println!("PASS live Developer implementation and real Cargo checks");

        // Inject a known fault to require a real finding/correction cycle even if
        // the first implementation was already correct. The reviewer is not told
        // which line or operator is wrong.
        std::fs::write(fixture.cwd().join("src/lib.rs"), BROKEN).unwrap();
        let before_reports = std::fs::read_dir(fixture.cwd().join(".cupel/reviews"))
            .unwrap()
            .map(|entry| {
                let path = entry.unwrap().path();
                let text = std::fs::read_to_string(&path).unwrap();
                (path, text)
            })
            .collect::<std::collections::BTreeMap<_, _>>();
        submit(&mut app, "/review --diff");
        let result = idle(&mut app).await;
        assert!(result.contains("CHANGES_REQUESTED"), "{result}");
        assert_eq!(
            std::fs::read_to_string(fixture.cwd().join("src/lib.rs")).unwrap(),
            BROKEN
        );
        let changed_reports: Vec<_> = std::fs::read_dir(fixture.cwd().join(".cupel/reviews"))
            .unwrap()
            .filter_map(|entry| {
                let path = entry.unwrap().path();
                let text = std::fs::read_to_string(&path).unwrap();
                (before_reports.get(&path) != Some(&text)).then_some((path, text))
            })
            .collect();
        assert_eq!(changed_reports.len(), 1, "one code review report required");
        let (path, original_findings) = &changed_reports[0];
        assert!(
            original_findings.contains("CHANGES_REQUESTED") && original_findings.contains("max"),
            "{original_findings}"
        );
        let report = path
            .strip_prefix(fixture.cwd())
            .unwrap()
            .to_string_lossy()
            .into_owned();
        println!("PASS live /review saves a real finding without editing source");

        let result = delegate(&mut app, Role::Implement,
        "Read the code review report, revalidate and fix every required finding in src/lib.rs. Append finding dispositions, run required checks, then request re-review. Preserve tests, the guide and the public signature.",
        &format!("Report: {report}; baseline HEAD. Acceptance: cap returns the smaller argument, including above/below/equal cases.")).await;
        assert!(result.contains("AWAITING_REVIEW"), "{result}");
        fixture_checks(&fixture);
        let before_recheck = std::fs::read(fixture.cwd().join("src/lib.rs")).unwrap();
        println!("PASS live Developer corrects reported findings and passes validation");

        let result = delegate(&mut app, Role::Review,
        "Re-check all required findings from the report against the current code and Developer dispositions. Run relevant tests, append dispositions and the final verdict to the same report; preserve original findings and source.",
        &format!("Report: {report}; baseline HEAD. Check smaller-argument behavior, unchanged public signature and unchanged tests.")).await;
        assert!(result.contains("APPROVED"), "{result}");
        assert_eq!(
            std::fs::read(fixture.cwd().join("src/lib.rs")).unwrap(),
            before_recheck
        );
        let final_report = std::fs::read_to_string(path).unwrap();
        assert!(
            final_report.starts_with(original_findings),
            "original review must be preserved"
        );
        assert!(final_report[original_findings.len()..].contains("APPROVED"));
        assert_eq!(
            std::fs::read(fixture.cwd().join("tests/cap.rs")).unwrap(),
            original_tests
        );
        assert_eq!(
            std::fs::read_to_string(fixture.cwd().join("IMPLEMENTATION.md")).unwrap(),
            GUIDE
        );
        let result = delegate(&mut app, Role::Implement,
        "Read the final independent approval and return the final task status and evidence. No further code edits are needed.",
        &format!("Report: {report}; required fixes and independent re-review are complete; Cargo checks passed.")).await;
        assert!(result.contains("COMPLETE"), "{result}");
        let usage = app.crew.total_spend();
        println!(
            "PASS live correction, independent re-review and final completion: {} helper calls, {} input, {} output, {} cached, ${:.6}",
            usage.calls, usage.input, usage.output, usage.cache_read, usage.cost
        );
    }
}
