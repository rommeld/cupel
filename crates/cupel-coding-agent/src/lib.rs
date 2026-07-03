//! The cupel coding agent: tools, search backends, and the system prompt.
//!
//! Rewrite of pi's `coding-agent` package, scoped to the first iteration:
//! only the `grep` tool, with the [`search::CodeSearch`] trait as the seam
//! where the `cupel-index` backend plugs in later (see
//! `crates/cupel-index/CONCEPT.md`).
//!
//! The `cupel` binary in `main.rs` wires everything into a minimal terminal
//! chat loop.

pub mod modes;
pub mod resources;
pub mod search;
pub mod system_prompt;
pub mod tools;
pub mod truncate;
