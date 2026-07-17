//! The cupel coding agent: tools, search backends, and the system prompt.
//!
//! The `cupel` binary in `main.rs` wires everything into a minimal terminal
//! chat loop.

pub mod commands;
pub mod guard;
pub mod hooks;
pub mod models;
pub mod modes;
pub mod ollama;
pub mod providers;
pub mod resources;
pub mod search;
pub mod session;
pub mod system_prompt;
pub mod tools;
pub mod truncate;
