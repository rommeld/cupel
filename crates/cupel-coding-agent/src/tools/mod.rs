//! Tools the coding agent exposes to the model.
//!
//! - [`read`]: file contents with offset/limit paging and image attachments
//! - [`grep`]: content search over the [`crate::search`] backend
//! - [`grep_rank`]: ranks files for grep's `files` output mode (definitions first, tests last)
//! - [`apply_patch`] - create/delete/rename/patch files with Codex's patch envelope
//!   ([`patch_parser`] reads it, [`patch_update`] applies update hunks to text,
//!   [`text_diff`] normalizes line endings and renders the transcript diff)
//! - [`bash`]: shell commands with bounded, tail-truncated output
//!
//! The mutating tool (`apply_patch`) serializes per file through
//! [`file_queue`] because the agent loop runs tool batches in parallel.
//!
//! `find` and `ls` are not separate tools because `bash` can do both.
//!
//! Note on permissions: tools execute without per-call user
//! approval. Project trust gates lifecycle hooks and sensitive model
//! configuration, not model-directed tool execution; it is not a sandbox.
//! An agent permission hook can veto calls via
//! [`AgentHooks::before_tool_call`](cupel_agent::AgentHooks::before_tool_call)
//! when a stricter policy is needed.

pub mod apply_patch;
pub mod bash;
pub mod file_queue;
pub mod grep;
pub mod grep_rank;
pub mod patch_parser;
pub mod patch_update;
pub mod read;
pub mod text_diff;
