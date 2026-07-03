//! Tools the coding agent exposes to the model.
//!
//! First iteration: only `grep`, per the plan. pi additionally ships `read`,
//! `bash`, `edit`, `write`, `find`, and `ls` - they follow in later
//! iterations using the same [`AgentTool`](cupel_agent::AgentTool) pattern
//! demonstrated here.

pub mod grep;
