//! Frontends ("modes") for the coding agent:
//!
//! - [`plain`]: a line-based REPL on a TTY, or one prompt from piped stdin
//!
//! The ratatui TUI lives in the `cupel-tui` crate, which builds on this
//! one. Both consume the same [`Agent`](cupel_agent::Agent); a mode is
//! purely a presentation layer over the agent's event stream.

pub mod plain;

/// Termination signals shared by the frontends so they can shut down cleanly.
/// On non-Unix platforms, reception stays pending.
pub struct TerminationSignals {
    #[cfg(unix)]
    term: tokio::signal::unix::Signal,
    #[cfg(unix)]
    hup: tokio::signal::unix::Signal,
    #[cfg(unix)]
    int: tokio::signal::unix::Signal,
}

impl TerminationSignals {
    /// Register handlers before entering a frontend's input loop.
    pub fn new() -> std::io::Result<Self> {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{SignalKind, signal};

            Ok(Self {
                term: signal(SignalKind::terminate())?,
                hup: signal(SignalKind::hangup())?,
                int: signal(SignalKind::interrupt())?,
            })
        }
        #[cfg(not(unix))]
        Ok(Self {})
    }

    /// Wait for SIGTERM, SIGHUP or SIGINT, returning its signal number.
    pub async fn recv(&mut self) -> i32 {
        #[cfg(unix)]
        {
            tokio::select! {
                _ = self.term.recv() => 15,
                _ = self.hup.recv() => 1,
                _ = self.int.recv() => 2,
            }
        }
        #[cfg(not(unix))]
        std::future::pending().await
    }
}

/// Static session info the frontends display (header/footer), plus the
/// command resources both frontends dispatch against.
pub struct SessionMeta {
    pub model_name: String,
    pub provider: String,
    pub cwd: String,
    /// `/name`-invocable prompt templates (see [`crate::commands`]).
    pub templates: Vec<crate::commands::PromptTemplate>,
    /// The merged model catalog (built-ins + models.json layers + ollama
    /// discovery), resolved once at startup by `main::run()`. Frontends
    /// read models from here, never from `cupel_core::catalog` directly.
    /// discovery is async and must not run inside sync key handlers.
    pub models: Vec<cupel_core::types::Model>,
    /// The resolved cupel home (`CUPEL_HOME` or `~/.cupel`). Threaded so
    /// runtime reloads (/hot-reload) rebuild from the same home the
    /// session started with, without environment access and easy to test.
    pub home: Option<std::path::PathBuf>,
    /// `~/.cupel/settings.json` as loaded at startup (or the last
    /// /hot-reload). The lowest key-precedence tier. See App::resolve_key
    /// (session-entered > env var > this).
    pub settings: crate::settings::Settings,
    /// A startup condition worth telling the user about (e.g. "no
    /// credentials found"). The TUI shows it as a transcript
    /// notice instead of refusing to start.
    pub startup_warning: Option<String>,
    /// Warnings from loading the session configuration. The TUI consumes
    /// these as transcript notices, including on runtime reloads.
    pub warnings: Vec<String>,
    /// The context files as loaded at session start (already embedded in
    /// the agent's system prompt). Bare `/hot-reload` diffs the files on
    /// disk against these and appends only the delta to the conversation.
    pub context_files: Vec<crate::resources::ContextFile>,
    /// The system prompt without a preset prompt, as bootstrap built it.
    /// `/preset` appends the chosen preset's prompt to this, so switching
    /// presets replaces the previous preset prompt instead of stacking it.
    pub base_system_prompt: String,
}
