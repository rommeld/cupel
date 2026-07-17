//! Session ingredients: everything a fresh agent session loads from disk,
//! assembled in ONE place so startup (`main::run`) and the TUI's
//! `/hot-reload` produce byte-identical results. Before this module the
//! assembly lived inline in main.rs - hot-reload would have had to copy
//! it, and the two paths would drift.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use cupel_agent::types::AgentTool;
use cupel_core::types::Model;

use crate::commands::PromptTemplate;
use crate::guard::BashGuard;
use crate::search::GrepSearch;
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
    /// Bash denylist rebuilt from the current bash-deny files.
    pub guard: BashGuard,
    /// settings.json layers (default model/thinking, usage limits).
    pub settings: crate::settings::Settings,
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
    let settings = crate::settings::load_settings(home.as_deref(), cwd);

    // The grep tool talks to a CodeSearch backend; today that's GrepSearch,
    // in iteration two an index-backed one from cupel-index slots in here.
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
        tools,
        templates,
        models,
        guard,
        settings,
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
            cwd.join(".cupel/settings.json"),
            r#"{"model": "local-test", "limits": {"maxCostUsd": 3.5}}"#,
        )
        .unwrap();

        let registry = cupel_core::default_registry();
        let ingredients = load(&cwd, Some(home), &registry).await;

        assert_eq!(ingredients.settings.model.as_deref(), Some("local-test"));
        assert_eq!(ingredients.settings.limits.max_cost_usd, Some(3.5));
        assert!(ingredients.system_prompt.contains("ALWAYS SAY PING"));
        assert!(ingredients.templates.iter().any(|t| t.name == "greet"));
        assert!(ingredients.models.iter().any(|m| m.id == "local-test"));
        assert_eq!(ingredients.tools.len(), 5);
        // The guard carries defaults AND the project rule.
        // (Verified through the public hook in guard.rs tests; here the
        // cheap signal is that construction succeeded with both layers.)
    }
}
