//! Tools the coding agent exposes to the model.
//!
//! - [`read`] - file contents with offset/limit paging and image attachments
//! - [`grep`] - content search over the [`crate::search`] backend
//! - [`edit`] - exact-text replacement with fuzzy fallback ([`edit_diff`])
//! - [`write`] - create/overwrite whole files
//! - [`bash`] - shell commands with bounded, tail-truncated output
//!
//! Mutating tools (`edit`, `write`) serialize per file through
//! [`file_queue`] because the agent loop runs tool batches in parallel.
//!
//! Still to port from pi: `find` and `ls` (both are convenience wrappers
//! over what `bash` can already do).
//!
//! Note on permissions: like pi, tools execute without per-call user
//! approval - the trust boundary is launching cupel in a directory at all.
//! A permission hook can veto calls via
//! [`AgentHooks::before_tool_call`](cupel_agent::AgentHooks::before_tool_call)
//! when a stricter policy is needed.

pub mod bash;
pub mod edit;
pub mod edit_diff;
pub mod file_queue;
pub mod grep;
pub mod read;
pub mod write;
