//! Frontends ("modes") for the coding agent,
//!
//! * [`plain`], a line based REPL on a TTY, or one prompt from piped stdin
//!
//! The ratatui TUI lives in the TUI crate, which builds on this
//! one. Both consume the same `Agent`, a mode is
//! purely a presentation layer over the agent's event stream.

pub mod plain;

/// Termination signals shared by the frontends so they can shut down cleanly.
/// On non Unix platforms, reception stays pending.
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
    pub templates: Vec<crate::commands::PromptTemplate>,
    pub models: Vec<cupel_core::types::Model>,
    pub home: Option<std::path::PathBuf>,
    pub settings: crate::settings::Settings,
    pub startup_warning: Option<String>,
    pub warnings: Vec<String>,
    pub context_files: Vec<crate::resources::ContextFile>,
    pub base_system_prompt: String,
}
