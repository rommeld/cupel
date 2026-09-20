//! Tools the coding agent exposes to the model.
//!
//! — [`read`] — file contents with offset/limit paging and image attachments
//! — [`grep`] — content search over the [`crate::search`] backend
//! — [`grep_rank`] — ranks files for grep's `files` output mode (definitions first, tests last)
//! - [`apply_patch`] - create/delete/rename/patch files with Codex's patch envelope
//!   ([`patch_parser`] reads it, [`patch_update`] applies update hunks to text,
//!   [`text_diff`] normalizes line endings and renders the transcript diff)
//! — [`bash`] — shell commands with bounded, tail-truncated output
//!
//! The mutating tool (`apply_patch`) serializes per file through
//! [`file_queue`] because the agent loop runs tool batches in parallel.
//!
//! Still to port from pi: `find` and `ls` (both are convenience wrappers
//! over what `bash` can already do).
//!
//! Note on permissions: like pi, tools execute without per-call user
//! approval — the trust boundary is launching cupel in a directory at all.
//! A permission hook can veto calls via
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
