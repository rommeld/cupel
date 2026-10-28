//! Opt-in, billable M8 verification against Fireworks. Never runs in normal CI.
//! Run with CUPEL_LIVE_AGENTS=1 cargo test -p cupel-tui --test live_agents -- --ignored --nocapture.

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::Duration;

    use cupel_agent::{Agent, AgentOptions, ToolExecutionMode};
    use cupel_coding_agent::bootstrap::{self, Frontend};
    use cupel_coding_agent::modes::SessionMeta;
    use cupel_coding_agent::session::SessionRecorder;
    use cupel_tui::app::{App, ReloadTarget, Status};
    use cupel_tui::startup::TeamInitialization;
    use cupel_tui::transcript::Cell;
    use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

    struct Workspace(PathBuf);

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
        tokio::time::timeout(Duration::from_secs(120), async {
            while app.is_running() {
                let event = app.next_event().await;
                app.on_event(event).await;
            }
        })
        .await
        .expect("live provider did not finish within two minutes");
        assert_ne!(app.last_answer, Status::Failed, "{}", transcript(app));
    }

    fn transcript(app: &App) -> String {
        app.transcript
            .cells
            .iter()
            .filter_map(|cell| match cell {
                Cell::Agent { text, .. }
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
        let root = std::env::temp_dir().join(format!(
            "cupel-live-m8-{}-{}",
            std::process::id(),
            cupel_core::types::now_ms()
        ));
        std::fs::create_dir_all(root.join("project/src")).unwrap();
        let root = Workspace(std::fs::canonicalize(root).unwrap());
        let cwd = root.0.join("project");
        let home = root.0.join("home");
        std::fs::create_dir_all(&home).unwrap();
        let settings = serde_json::json!({
            "checkForUpdates":false,
            "providers":{"fireworks":key},
            "agents":{
                "map-reader":{"provider":"fireworks","model":"accounts/fireworks/models/minimax-m3p1-flash","thinkingLevel":"off","description":"Finds file definitions and transcript paths. Read-only.","remember":false,"maxTurns":4},
                "diff-auditor":{"provider":"fireworks","model":"accounts/fireworks/models/minimax-m3","thinkingLevel":"low","description":"Reviews diffs and performs small edits or shell tasks explicitly requested by the user.","tools":["read","grep","bash"],"maxTurns":20}
            },
            "agentCommands":{"review":"diff-auditor"}
        });
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
        let registry = Arc::new(cupel_core::default_registry());
        let mut ingredients =
            bootstrap::load(&cwd, Some(home.clone()), &registry, Frontend::Tui).await;
        // Bound each request, independently of provider catalog output limits.
        for model in &mut ingredients.models {
            model.max_tokens = model.max_tokens.min(1024);
        }
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
        options.max_tokens = Some(1024);
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
            "Use delegate to ask diff-auditor to run exactly: echo started > waiting.txt; sleep 90. This cancellation test needs that bash command, not a simulated sleep. Do nothing else until its tool returns.",
        );
        if let Some(prompt) = app.pending_prompt.take() {
            app.recorder.before_prompt(&prompt).await;
            app.start_run(&prompt);
        }
        tokio::time::timeout(Duration::from_secs(120), async {
            while !cwd.join("waiting.txt").exists() {
                let event = app.next_event().await;
                app.on_event(event).await;
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
        let mut resumed = App::new(
            Agent::new(options),
            app.meta,
            SessionRecorder::new(Some(home), &cwd, "live-m8", &model.id),
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
        let helpers = resumed.crew.total_spend();
        println!(
            "Fireworks helpers: {} calls, {} input, {} output, {} cached, ${:.6}",
            helpers.calls, helpers.input, helpers.output, helpers.cache_read, helpers.cost
        );
        println!(
            "Fireworks combined: {} input, {} output, {} cached, ${:.6}",
            total.input, total.output, total.cache_read, total.cost
        );
    }
}
