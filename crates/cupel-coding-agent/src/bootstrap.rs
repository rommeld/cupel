//! Session ingredients, everything a fresh agent session loads from disk,
//! assembled in one place so startup (`run`) and the TUI's
//! hot reload produce byte identical results. Before this module the
//! assembly lived inline in main.rs. Hot reload would have had to copy
//! it, and the two paths would drift.

use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cupel_agent::AgentHooks;
use cupel_agent::types::{AgentTool, BeforeToolCallResult};
use cupel_core::types::{AssistantMessage, Model, ToolCall};

use crate::commands::PromptTemplate;
use crate::guard::BashGuard;
use crate::jobs::Jobs;
use crate::loop_killer::LoopKiller;
use crate::search::GrepSearch;
use crate::settings::Settings;
use crate::skills::{self, Skill};
use crate::source_observations::SourceObservations;
use crate::system_prompt;
use crate::tools::{
    apply_patch::ApplyPatchTool,
    ask_user::{AskUserTool, QuestionReceiver},
    bash::BashTool,
    grep::GrepTool,
    jobs::JobsTool,
    read::ReadTool,
};

/// Which frontend a session runs in. Only the TUI can answer a question while a run
/// waits for it, so only the TUI gets the `ask_user` tool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Frontend {
    Plain,
    Tui,
}

/// The reloadable parts of a session, all derived from `~/.cupel` +
/// `<cwd>/.cupel` + the cwd itself.
pub struct Ingredients {
    pub base_system_prompt: String,
    pub skills: Vec<Skill>,
    pub tools: Vec<Arc<dyn AgentTool>>,
    pub templates: Vec<PromptTemplate>,
    pub models: Vec<Model>,
    pub hooks: SessionHooks,
    pub settings: Settings,
    pub context_files: Vec<crate::resources::ContextFile>,
    pub warnings: Vec<String>,
    pub questions: Option<QuestionReceiver>,
}

impl Ingredients {
    /// The startup prompt, including skill metadata but not skill instructions.
    #[must_use]
    pub fn system_prompt(&self) -> String {
        system_prompt::with_skill_catalog(&self.base_system_prompt, &self.skills)
    }

    /// Startup and reload bind tools to the recorder's journal and live source
    /// observations. Unbound tools remain useful for standalone callers/tests.
    pub fn bind_session(&mut self, recorder: &crate::session::SessionRecorder, cwd: &Path) {
        let undo = recorder.undo();
        let observations = recorder.source_observations();
        let jobs = recorder.jobs();
        for tool in &mut self.tools {
            match tool.name() {
                "apply_patch" => {
                    *tool = Arc::new(
                        ApplyPatchTool::new(cwd)
                            .with_undo(Arc::clone(&undo))
                            .with_observations(Arc::clone(&observations)),
                    );
                }
                "read" => {
                    *tool =
                        Arc::new(ReadTool::new(cwd).with_observations(Arc::clone(&observations)));
                }
                "bash" => *tool = Arc::new(BashTool::new(cwd).with_jobs(Arc::clone(&jobs))),
                "jobs" => *tool = Arc::new(JobsTool::new(Arc::clone(&jobs))),
                _ => {}
            }
        }
        self.hooks.undo = Some(undo);
    }
}

/// Load every ingredient fresh from disk (and the bounded ollama probe).
/// Parameterized on `home` instead of reading `CUPEL_HOME` so hot reload
/// and tests share the exact code path with startup.
pub async fn load(
    cwd: &Path,
    home: Option<PathBuf>,
    registry: &cupel_core::provider::Registry,
    frontend: Frontend,
) -> Ingredients {
    let roots = crate::resources::roots_for(home.clone(), cwd);
    let context_files = crate::resources::load_context_files(&roots);
    let templates = crate::commands::load_prompt_templates(&roots);
    let (skills, skill_warnings) = skills::discover(home.as_deref(), cwd);
    let (home_settings, mut warnings) = crate::settings::load_home_settings(home.as_deref());
    warnings.extend(skill_warnings);
    let (project_settings, project_warnings) = crate::settings::load_project_settings(cwd);
    warnings.extend(project_warnings);
    warnings.extend(crate::settings::project_settings_warning(cwd));
    let (models, model_warnings) =
        crate::models::build_catalog(registry, home.as_deref(), cwd, &home_settings).await;
    warnings.extend(model_warnings);
    let (guard, guard_warnings) = BashGuard::from_config(home.as_deref(), cwd);
    warnings.extend(guard_warnings);
    let settings = Settings::layered(home_settings, project_settings);
    let hooks = SessionHooks::new(
        guard,
        LoopKiller::new(settings.loop_killer_max_repeats()),
        home.clone(),
    );

    let mut tools = coding_tools(cwd);
    let questions = match frontend {
        Frontend::Tui => {
            let (tool, questions) = AskUserTool::new();
            tools.push(Arc::new(tool));
            Some(questions)
        }
        Frontend::Plain => None,
    };
    // Full descriptions travel in the tool schemas.
    let tool_names: Vec<&str> = tools.iter().map(|tool| tool.name()).collect();

    Ingredients {
        base_system_prompt: system_prompt::build_system_prompt(cwd, &tool_names, &context_files),
        skills,
        context_files,
        tools,
        templates,
        models,
        hooks,
        settings,
        warnings,
        questions,
    }
}

/// The coding tools available to a session.
#[must_use]
pub fn coding_tools(cwd: &Path) -> Vec<Arc<dyn AgentTool>> {
    let backend = Arc::new(GrepSearch::new(cwd));
    let observations = Arc::new(SourceObservations::new());
    let jobs = Arc::new(Jobs::default());
    vec![
        Arc::new(ReadTool::new(cwd).with_observations(Arc::clone(&observations))),
        Arc::new(BashTool::new(cwd).with_jobs(Arc::clone(&jobs))),
        Arc::new(ApplyPatchTool::new(cwd).with_observations(observations)),
        Arc::new(GrepTool::new(cwd, backend)),
        Arc::new(JobsTool::new(jobs)),
    ]
}

/// The one [`AgentHooks`] object a session runs with, the bash guard and
/// the loop killer behind a single trait object (`AgentOptions.hooks`
/// holds exactly one). Fans out only `before_tool_call`, the sole hook
/// either member cares about, every other AgentHooks method keeps its
/// trait default. Extend the fan out when a member grwos a new hook.
pub struct SessionHooks {
    guard: BashGuard,
    loop_killer: LoopKiller,
    home: Option<PathBuf>,
    http: reqwest::Client,
    undo: Option<Arc<crate::undo::UndoJournal>>,
}

impl SessionHooks {
    #[must_use]
    pub fn new(guard: BashGuard, loop_killer: LoopKiller, home: Option<PathBuf>) -> Self {
        Self {
            guard,
            loop_killer,
            home,
            http: reqwest::Client::new(),
            undo: None,
        }
    }
}

#[async_trait]
impl AgentHooks for SessionHooks {
    async fn on_user_messages(&self, messages: &[cupel_agent::AgentMessage]) {
        if messages.iter().any(|message| {
            matches!(
                message,
                cupel_agent::AgentMessage::Llm(cupel_core::types::Message::User(_))
            )
        }) && let Some(undo) = &self.undo
        {
            undo.begin_turn().await;
        }
    }

    /// The agent loop calls this before every provider request (types.rs,
    /// "Resolve an API key for a provider right before each call"). The
    /// designed home for expiring OAuth tokens. Key based providers
    /// return None here and keep riding AgentOptions.api_key.
    async fn api_key(&self, provider: &str) -> Option<String> {
        if provider != cupel_core::types::Provider::OPENAI_CODEX {
            return None;
        }
        crate::auth::openai_codex_access_token(self.home.as_deref(), &self.http).await
    }
    async fn before_tool_call(
        &self,
        assistant: &AssistantMessage,
        tool_call: &ToolCall,
    ) -> Option<BeforeToolCallResult> {
        // The killer observes every request call first, including ones
        // the guard is about to veto, a model hammering a denied bash
        // command is looping too, and each guard block would otherwise
        // reset nothing while the loop spins forever. Once the killer
        // trips, its message takes over, "take a different approach"
        // redirects harder than the guard's per command veto, which is
        // exactly what a stuck model needs to hear.
        if let Some(reason) = self.loop_killer.note_call(tool_call) {
            return Some(BeforeToolCallResult {
                block: true,
                reason: Some(reason),
            });
        }
        self.guard.before_tool_call(assistant, tool_call).await
    }
}

#[cfg(test)]
mod tests {

    use super::*;

    #[tokio::test]
    async fn load_picks_up_every_cupel_layer() {
        let root = std::env::temp_dir().join("cupel-bootstrap-load");
        let _ = std::fs::remove_dir_all(&root);
        let (home, cwd) = (root.join("home"), root.join("proj"));
        std::fs::create_dir_all(home.join("prompts")).unwrap();
        std::fs::create_dir_all(cwd.join(".cupel")).unwrap();
        std::fs::write(home.join("AGENTS.md"), "ALWAYS SAY PING").unwrap();
        std::fs::write(home.join("prompts/greet.md"), "Greet $1.").unwrap();
        std::fs::write(
            cwd.join(".cupel/settings.json"),
            r#"{"loopKiller": {"maxRepeats": 2}}"#,
        )
        .unwrap();
        std::fs::write(
            cwd.join(".cupel/models.json"),
            serde_json::json!([{
                "id": "local-test", "name": "Local", "api": "openai-completions",
                "provider": "test-local", "baseUrl": "http://localhost:9/v1",
                "reasoning": false, "input": ["text"],
                "cost": {"input": 0, "output": 0, "cachedRead": 0, "cachedWrite": 0},
                "contextWindow": 4096, "maxTokens": 4096,
                "compat": {"requiresApiKey": false}
            }])
            .to_string(),
        )
        .unwrap();
        std::fs::write(cwd.join(".cupel/bash-deny"), "git\\s+push\\s+--force\n").unwrap();
        std::fs::write(
            home.join("settings.json"),
            r#"{"providers": {"test-local": "k-1"}}"#,
        )
        .unwrap();

        let registry = cupel_core::default_registry();
        let restricted = load(&cwd, Some(home.clone()), &registry, Frontend::Plain).await;
        assert!(!restricted.models.iter().any(|m| m.id == "local-test"));
        crate::project_trust::save(&home, &cwd, crate::project_trust::ProjectTrust::Trusted)
            .unwrap();
        let ingredients = load(&cwd, Some(home), &registry, Frontend::Plain).await;

        assert!(ingredients.system_prompt().contains("ALWAYS SAY PING"));
        assert!(ingredients.templates.iter().any(|t| t.name == "greet"));
        assert!(ingredients.models.iter().any(|m| m.id == "local-test"));
        assert_eq!(ingredients.tools.len(), 5);
        assert!(ingredients.warnings.is_empty());
        assert_eq!(ingredients.settings.api_key("test-local"), Some("k-1"));
        assert_eq!(ingredients.settings.loop_killer_max_repeats(), Some(2));
        assert_eq!(ingredients.settings.api_key("test-local"), Some("k-1"));
        // The guard carries defaults and the project rule.
        // (Verified through the public hook in guard.rs tests, here the
        // cheap signal is that construction succeeded with both layers.)
    }

    #[tokio::test]
    async fn load_collects_warnings_from_both_config_layers_once() {
        let root = std::env::temp_dir().join("cupel-bootstrap-warnings");
        let _ = std::fs::remove_dir_all(&root);
        let (home, cwd) = (root.join("home"), root.join("proj"));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(cwd.join(".cupel")).unwrap();
        let broken_files = [
            home.join("settings.json"),
            cwd.join(".cupel/settings.json"),
            home.join("models.json"),
            cwd.join(".cupel/models.json"),
        ];
        for path in &broken_files {
            std::fs::write(path, "{broken").unwrap();
        }
        std::fs::write(home.join("bash-deny"), "[home-rule\n").unwrap();
        std::fs::write(cwd.join(".cupel/bash-deny"), "[project-rule\n").unwrap();

        let registry = cupel_core::default_registry();
        let ingredients = load(&cwd, Some(home.clone()), &registry, Frontend::Plain).await;
        assert_eq!(ingredients.settings, Settings::default());
        assert_eq!(ingredients.warnings.len(), 6, "{:?}", ingredients.warnings);
        for path in broken_files {
            assert_eq!(
                ingredients
                    .warnings
                    .iter()
                    .filter(|warning| { warning.contains(&path.display().to_string()) })
                    .count(),
                1,
                "each broken layer is reported once"
            );
        }
        for pattern in ["[home-rule", "[project-rule"] {
            assert!(
                ingredients
                    .warnings
                    .iter()
                    .any(|warning| warning.contains(pattern))
            );
        }

        // Project credentials remain ignored and the notice never includes keys.
        std::fs::write(
            cwd.join(".cupel/settings.json"),
            r#"{"providers":{"fixture":"do-not-display-this-key"}}"#,
        )
        .unwrap();
        let ingredients = load(&cwd, Some(home), &registry, Frontend::Plain).await;
        assert_eq!(ingredients.settings.api_key("fixture"), None);
        assert!(
            ingredients
                .warnings
                .iter()
                .any(|warning| warning.contains("API keys belong"))
        );
        assert!(
            ingredients
                .warnings
                .iter()
                .all(|warning| !warning.contains("do-not-display-this-key"))
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn only_the_tui_gets_the_ask_user_tool() {
        let root = std::env::temp_dir().join("cupel-bootstrap-ask-user");
        let _ = std::fs::remove_dir_all(&root);
        let (home, cwd) = (root.join("home"), root.join("proj"));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        let registry = cupel_core::default_registry();

        let tui = load(&cwd, Some(home.clone()), &registry, Frontend::Tui).await;
        assert!(tui.tools.iter().any(|tool| tool.name() == "ask_user"));
        assert!(tui.questions.is_some());
        assert!(tui.system_prompt().contains("\n- ask_user\n\nGuidelines:"));
        assert!(tui.system_prompt().contains("\n- Use ask_user "));

        let plain = load(&cwd, Some(home), &registry, Frontend::Plain).await;
        assert!(plain.tools.iter().all(|tool| tool.name() != "ask_user"));
        assert!(plain.questions.is_none());
        assert!(!plain.system_prompt().contains("ask_user"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn managed_jobs_report_exit_and_output_and_reset_without_a_transcript() {
        use std::time::Duration;

        use serde_json::json;
        use tokio_util::sync::CancellationToken;

        async fn call(
            tool: &dyn AgentTool,
            args: serde_json::Value,
        ) -> cupel_agent::AgentToolResult {
            tool.execute("test", args, CancellationToken::new(), None)
                .await
                .unwrap()
        }

        let cwd = std::env::temp_dir().join(format!("cupel-bootstrap-jobs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&cwd);
        std::fs::create_dir_all(&cwd).unwrap();
        let registry = cupel_core::default_registry();
        let mut ingredients = load(&cwd, None, &registry, Frontend::Plain).await;
        let mut recorder = crate::session::SessionRecorder::new(None, &cwd, "test-jobs", "mock");
        ingredients.bind_session(&recorder, &cwd);
        let bash = ingredients
            .tools
            .iter()
            .find(|tool| tool.name() == "bash")
            .unwrap();
        let jobs = ingredients
            .tools
            .iter()
            .find(|tool| tool.name() == "jobs")
            .unwrap();

        let start = tokio::time::timeout(Duration::from_secs(5), call(bash.as_ref(), json!({
            "command": "while [ ! -f release ]; do sleep 0.05; done; printf stdout; printf stderr >&2; exit 7",
            "background": true,
            "timeout": 20,
        }))).await.unwrap();
        let details = start.details.unwrap();
        let id = details["id"].as_str().unwrap();
        assert!(
            recorder.jobs().require_idle().is_err(),
            "job must outlive its start call"
        );
        call(bash.as_ref(), json!({"command": ": > release"})).await;

        let output = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let result = call(jobs.as_ref(), json!({"action":"output", "id":id})).await;
                match result.details.as_ref().unwrap()["state"].as_str() {
                    Some("running" | "stopping") => {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                    _ => break result,
                }
            }
        })
        .await
        .unwrap();
        let status = output.details.unwrap();
        assert_eq!(status["state"], "exited");
        assert_eq!(status["exit"]["exitCode"], 7);
        assert!(
            matches!(&output.content[0], cupel_core::types::ToolResultContent::Text(text)
            if text.text.contains("stdout") && text.text.contains("stderr"))
        );

        let start = call(
            bash.as_ref(),
            json!({"command":"sleep 30", "background":true}),
        )
        .await;
        let details = start.details.unwrap();
        let live_id = details["id"].as_str().unwrap();
        let old_jobs = recorder.jobs();
        tokio::time::timeout(Duration::from_secs(5), recorder.reset_jobs())
            .await
            .unwrap();
        assert!(
            old_jobs.require_idle().is_ok(),
            "reset waits for the running process to stop"
        );
        for id in [live_id, id] {
            let result = jobs
                .execute(
                    "old-job",
                    json!({"action":"status", "id":id}),
                    CancellationToken::new(),
                    None,
                )
                .await;
            assert!(result.is_err(), "reset invalidated {id}");
        }
        std::fs::remove_dir_all(cwd).unwrap();
    }

    #[tokio::test]
    async fn skills_are_discovered_as_metadata_and_read_on_demand() {
        use cupel_core::types::ToolResultContent;
        use tokio_util::sync::CancellationToken;

        let root =
            std::env::temp_dir().join(format!("cupel-bootstrap-skills-{}", std::process::id()));
        let (home, cwd) = (root.join("home"), root.join("proj"));
        let fixtures: [(&Path, &str, &[u8]); 7] = [
            (&home, "skills/review/SKILL.md", b"---\nname: review-pr\ndescription: Global review\n---\nGlobal body"),
            (&home, "skills/format/SKILL.md", b"---\nname: format\ndescription: Global formatting\n---\n\xff"),
            (&cwd, ".cupel/skills/local-review/SKILL.md", b"---\r\nname: review-pr\r\ndescription: >\r\n  Review for regressions\r\n  and missing tests.\r\n---\r\nProject review body"),
            (&cwd, ".cupel/skills/broken-format/SKILL.md", b"---\nname: format\ndescription: [invalid\n---\n"),
            (&cwd, ".cupel/skills/unfinished/SKILL.md", b"---\nname: unfinished\ndescription: Unfinished header\n---not-a-fence\n"),
            (&cwd, ".cupel/skills/blank/SKILL.md", b"---\nname: blank\ndescription: '  '\n---\n"),
            (&cwd, "skills/not-a-root/SKILL.md", b"---\nname: not-a-root\ndescription: Not a skill root\n---\n"),
        ];
        for (base, relative_path, content) in fixtures {
            let path = base.join(relative_path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, content).unwrap();
        }

        let registry = cupel_core::default_registry();
        let ingredients = load(&cwd, Some(home), &registry, Frontend::Plain).await;
        assert_eq!(ingredients.skills.len(), 2);
        let prompt = ingredients.system_prompt();
        assert!(
            prompt.contains("Global formatting"),
            "the invalid project override must not hide the home skill"
        );
        assert!(prompt.contains("Review for regressions and missing tests."));
        for excluded in [
            "Global review",
            "Global body",
            "Project review body",
            "not-a-root",
        ] {
            assert!(
                !prompt.contains(excluded),
                "unexpected in startup prompt: {excluded}"
            );
        }
        assert_eq!(ingredients.warnings.len(), 3, "{:?}", ingredients.warnings);
        for invalid in ["broken-format", "unfinished", "blank"] {
            assert!(
                ingredients
                    .warnings
                    .iter()
                    .any(|warning| warning.contains(invalid))
            );
        }

        let selected = prompt
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .find(|entry| entry["name"] == "review-pr")
            .unwrap();
        let path = selected["path"].as_str().unwrap();
        assert!(Path::new(path).is_absolute());
        let reader = ingredients
            .tools
            .iter()
            .find(|tool| tool.name() == "read")
            .unwrap();
        let result = reader
            .execute(
                "read-skill",
                serde_json::json!({"path": path}),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();
        assert!(
            matches!(&result.content[0], ToolResultContent::Text(text) if text.text.contains("Project review body"))
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn binding_preserves_observations_across_reload_but_not_resume() {
        use serde_json::json;
        use tokio_util::sync::CancellationToken;

        let root = std::env::temp_dir().join(format!(
            "cupel-bootstrap-observations-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("file"), "old\n").unwrap();
        let registry = cupel_core::default_registry();
        let mut ingredients =
            load(&root, Some(root.join("home")), &registry, Frontend::Plain).await;
        let recorder = crate::session::SessionRecorder::new(None, &root, "same-session", "mock");
        ingredients.bind_session(&recorder, &root);
        let reader = ingredients
            .tools
            .iter()
            .find(|tool| tool.name() == "read")
            .unwrap();
        let read = reader
            .execute(
                "read",
                json!({"path": "file"}),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();
        let id = read.details.unwrap()["observation"]
            .as_str()
            .unwrap()
            .to_string();

        ingredients.tools = coding_tools(&root);
        ingredients.bind_session(&recorder, &root);
        let patcher = ingredients
            .tools
            .iter()
            .find(|tool| tool.name() == "apply_patch")
            .unwrap();
        let input = "*** Begin Patch\n*** Update File: file\n@@\n-old\n+new\n*** End Patch";
        patcher
            .execute(
                "patch",
                json!({"input": input, "expectedObservations": [id.clone()]}),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(root.join("file")).unwrap(), "new\n");

        let resumed = crate::session::SessionRecorder::new(None, &root, "same-session", "mock");
        ingredients.bind_session(&resumed, &root);
        let patcher = ingredients
            .tools
            .iter()
            .find(|tool| tool.name() == "apply_patch")
            .unwrap();
        let error = patcher
            .execute(
                "patch",
                json!({"input": input, "expectedObservations": [id]}),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("unavailable or expired"), "{error}");
        assert_eq!(std::fs::read_to_string(root.join("file")).unwrap(), "new\n");
        std::fs::remove_dir_all(root).unwrap();
    }
}
