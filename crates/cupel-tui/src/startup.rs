//! Startup actions dispatched through the ordinary TUI handlers.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartupCommand {
    Review,
}
