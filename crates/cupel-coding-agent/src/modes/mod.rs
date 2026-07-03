//! Frontends ("modes") for the coding agent, mirroring pi's `modes/` layout:
//!
//! - [`interactive`] - the ratatui TUI (default when stdout is a terminal)
//! - [`plain`] - a line-based REPL (for pipes, dumb terminals, and `--plain`)
//!
//! Both consume the same [`Agent`](cupel_agent::Agent); a mode is purely a
//! presentation layer over the agent's event stream.

pub mod interactive;
pub mod plain;

/// Static session info the frontends display (header/footer).
pub struct SessionMeta {
    pub model_name: String,
    pub provider: String,
    pub cwd: String,
}
