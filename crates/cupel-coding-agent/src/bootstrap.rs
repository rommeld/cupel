//! Session ingredients: everything a fresh agent session loads from disk,
//! assembled in ONE place so startup (`main::run`) and the TUI's
//! `/hot-reload` produce byte-identical results. Before this module the
//! assembly lived inline in main.rs - hot-reload would have had to copy
//! it, and the two paths would drift.

use async_trait::async_trait;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cupel_agent::AgentHooks;
use cupel_agent::types::{AgentTool, BeforeToolCallResult};
use cupel_core::types::{AssistantMessage, Model, ToolCall};

use crate::commands::PromptTemplate;
use crate::guard::BashGuard;
use crate::loop_killer::LoopKiller;
use crate::search::GrepSearch;
use crate::settings::Settings;
use crate::system_prompt::build_system_prompt;
use crate::tools::{
    bash::BashTool, edit::EditTool, grep::GrepTool, read::ReadTool, write::WriteTool,
};

/// Name + one-line snippet per tool for the system prompt (full
/// descriptions travel in the tool schemas).
pub const TOOL_SUMMARIES: &[(&str, &str)] = &[
    ("read", "Read file contents"),
    ("bash", "Execute bash commands (ls, find, cargo, etc.)"),
    (
        "edit",
        "Make precise file edits with exact text replacement, including multiple disjoint edits \
         in one call",
    ),
    ("write", "Create or overwrite files"),
    (
        "grep",
        "Search file contents for patterns (respects .gitignore)",
    ),
];

/// The reloadable parts of a session, all derived from `~/.cupel` +
/// `<cwd>/.cupel` + the cwd itself.
pub struct Ingredients {
    /// System prompt incl. freshly read AGENTS.md/CLAUDE.md context files.
    pub system_prompt: String,
    pub tools: Vec<Arc<dyn AgentTool>>,
    /// `/name` prompt templates from `<root>/prompts/*.md`.
    pub templates: Vec<PromptTemplate>,
    /// Merged model catalog (builtins + models.json + ollama discovery).
    pub models: Vec<Model>,
    /// The agent-loop hooks (bash guard + loop killer + ...), rebuild per
    /// load so /hot-reload picks up both bash-deny edits and a changed
    /// loopKiller setting.
    pub hooks: SessionHooks,
    /// Layered settings: ~/.cupel/settings.json overlaid with the
    /// project's .cupel/settings.json (non-secret fields only -provider
    /// keys stay home-only by construction). Loaded here so /hot-reload
    /// picks up hand edits to either file.
    pub settings: Settings,
    /// The context files (AGENTS.md/CLAUDE.md) as loaded - kept alongside
    /// the system prompt they were embedded into, so a later in-place
    /// `/hot-reload` can DIFF against them instead of re-reading blind.
    pub context_files: Vec<crate::resources::ContextFile>,
}

/// Load every ingredient fresh from disk (and the bounded ollama probe).
/// Parameterized on `home` instead of reading `CUPEL_HOME` so hot-reload
/// and tests share the exact code path with startup.
pub async fn load(
    cwd: &Path,
    home: Option<PathBuf>,
    registry: &cupel_core::provider::Registry,
) -> Ingredients {
    let roots = crate::resources::roots_for(home.clone(), cwd);
    let context_files = crate::resources::load_context_files(&roots);
    let templates = crate::commands::load_prompt_templates(&roots);
    let models = crate::models::build_catalog(registry, home.as_deref(), cwd).await;
    let guard = BashGuard::from_config(home.as_deref(), cwd);
    let settings = Settings::layered(
        crate::settings::load_home_settings(home.as_deref()),
        crate::settings::load_project_settings(cwd),
    );
    let hooks = SessionHooks::new(
        guard,
        LoopKiller::new(settings.loop_killer_max_repeats()),
        home.clone(),
    );
    // A project-side settings.json must never hold keys - warn once here,
    // on the same stderr channel as the models.json warnings.
    crate::settings::warn_project_settings(cwd);

    // The grep tool talks to a CodeSearch backend.
    let backend = Arc::new(GrepSearch::new(cwd));
    let tools: Vec<Arc<dyn AgentTool>> = vec![
        Arc::new(ReadTool::new(cwd)),
        Arc::new(BashTool::new(cwd)),
        Arc::new(EditTool::new(cwd)),
        Arc::new(WriteTool::new(cwd)),
        Arc::new(GrepTool::new(cwd, backend)),
    ];

    Ingredients {
        system_prompt: build_system_prompt(cwd, TOOL_SUMMARIES, &context_files),
        context_files,
        tools,
        templates,
        models,
        hooks,
        settings,
    }
}

/// The ONE [`AgentHooks`] object a session run with: the bash guard and
/// the loop killer behind a single trait object (`AgentOptions.hooks`
/// hols exactly one). Fans out only `before_tool_call` - the sole hook
/// either member cares about; every other AgentHooks method keeps its
/// trait default. Extend the fan-out when a member grwos a new hook.
pub struct SessionHooks {
    guard: BashGuard,
    loop_killer: LoopKiller,
    /// Where auth.json lives - the api_key hook resolves subscription
    /// tokens per call (refreshing them near expiry).
    home: Option<PathBuf>,
    /// One shared client for those refresh calls.
    http: reqwest::Client,
}

impl SessionHooks {
    #[must_use]
    pub fn new(guard: BashGuard, loop_killer: LoopKiller, home: Option<PathBuf>) -> Self {
        Self {
            guard,
            loop_killer,
            home,
            http: reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl AgentHooks for SessionHooks {
    /// The agent loop calls this before EVERY provider request (types.rs:
    /// "Resolve an API key for a provider right before each call") - the
    /// designed home for expiring OAuth tokens. Key-based providers
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
        // The killer OBSERVES every request call FIRST - including ones
        // the guard is about to veto: a model hammering a denied bash
        // command is looping too, and each guard block would otherwise
        // reset nothing while the loop spins forever. Once the killer
        // trips, its message takes over: "take a different approach"
        // redirects harder than the guard's per-command veto, which is
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
        let ingredients = load(&cwd, Some(home), &registry).await;

        assert!(ingredients.system_prompt.contains("ALWAYS SAY PING"));
        assert!(ingredients.templates.iter().any(|t| t.name == "greet"));
        assert!(ingredients.models.iter().any(|m| m.id == "local-test"));
        assert_eq!(ingredients.tools.len(), 5);
        assert_eq!(ingredients.settings.api_key("test-local"), Some("k-1"));
        assert_eq!(ingredients.settings.loop_killer_max_repeats(), Some(2));
        assert_eq!(ingredients.settings.api_key("test-local"), Some("k-1"));
        // The guard carries defaults AND the project rule.
        // (Verified through the public hook in guard.rs tests; here the
        // cheap signal is that construction succeeded with both layers.)
    }
}
