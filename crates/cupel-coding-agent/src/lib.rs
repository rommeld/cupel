//! The cupel coding agent: tools, search backends, and the system prompt.
//!
//! A library without any UI dependency. The `cupel` binary and its ratatui
//! TUI live in the `cupel-tui` crate, which builds on this one.

pub mod auth;
pub mod bootstrap;
pub mod commands;
pub mod guard;
pub mod hooks;
pub mod loop_killer;
pub mod models;
pub mod modes;
pub mod ollama;
mod process;
pub mod project_trust;
pub mod providers;
pub mod resources;
pub mod review;
pub mod search;
pub mod session;
pub mod settings;
pub mod spinoff;
pub mod system_prompt;
pub mod tools;
pub mod truncate;
