//! Frontends ("modes") for the coding agent, mirroring pi's `modes/` layout:
//!
//! - [`interactive`] - the ratatui TUI (default when stdout is a terminal)
//! - [`plain`] - a line-based REPL (for pipes, dumb terminals, and `--plain`)
//!
//! Both consume the same [`Agent`](cupel_agent::Agent); a mode is purely a
//! presentation layer over the agent's event stream.

pub mod interactive;
pub mod plain;

/// Static session info the frontends display (header/footer), plus the
/// command resources both frontends dispatch against.
pub struct SessionMeta {
    pub model_name: String,
    pub provider: String,
    pub cwd: String,
    /// `/name`-invocable prompt templates (see [`crate::commands`]).
    pub templates: Vec<crate::commands::PromptTemplate>,
    /// The merged model catalog (built-ins + models.json layers + ollama
    /// discovery), resolved ONCE at startup by `main::run()`. Frontends
    /// read models from here, never from `cupel_core::catalog` directly -
    /// discovery is async and must not run inside sync key handlers.
    pub models: Vec<cupel_core::types::Model>,
    /// The resolved cupel home (`CUPEL_HOME` or `~/.cupel`). Threaded so
    /// runtime reloads (/hot-reload) rebuild from the SAME home the
    /// session started with - env-free and testable.
    pub home: Option<std::path::PathBuf>,
}
