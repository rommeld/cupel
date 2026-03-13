use std::collections::HashMap;
use std::path::Path;

use crate::git::error::GitResult;
use crate::git::types::{
    BlameEntry, Branch, CommitDetails, CommitOptions, CommitSummary, ConflictSide, PushOptions,
    RepoPath, StashEntry, StatusEntry,
};

/// Synchronous, Send + Sync trait for all git operations.
///
/// Implementations wrap either git2 (for local operations) or the git CLI
/// (for network operations). The async wrapper lives in the Repository entity,
/// not here.
pub trait GitRepository: Send + Sync {
    // ── Identity ───────────────────────────────────────────
    fn path(&self) -> &Path;
    fn work_directory(&self) -> Option<&Path>;

    // ── Staging / Index ────────────────────────────────────
    fn stage_paths(
        &self,
        paths: &[RepoPath],
        env: &HashMap<String, String>,
    ) -> GitResult<()>;

    fn unstage_paths(
        &self,
        paths: &[RepoPath],
        env: &HashMap<String, String>,
    ) -> GitResult<()>;

    fn set_index_text(
        &self,
        path: &RepoPath,
        content: Option<String>,
        env: &HashMap<String, String>,
    ) -> GitResult<()>;

    fn reload_index(&self);

    // ── Commit ─────────────────────────────────────────────
    fn commit(
        &self,
        message: &str,
        options: &CommitOptions,
        env: &HashMap<String, String>,
    ) -> GitResult<()>;

    fn uncommit(&self, env: &HashMap<String, String>) -> GitResult<()>;

    // ── Remote operations ──────────────────────────────────
    fn push(
        &self,
        branch: &str,
        remote: Option<&str>,
        options: &PushOptions,
        env: &HashMap<String, String>,
    ) -> GitResult<()>;

    fn pull(
        &self,
        rebase: bool,
        env: &HashMap<String, String>,
    ) -> GitResult<()>;

    fn fetch(&self, env: &HashMap<String, String>) -> GitResult<()>;

    fn create_remote(&self, name: &str, url: &str) -> GitResult<()>;

    // ── Status ─────────────────────────────────────────────
    fn status(&self, path_prefixes: &[RepoPath]) -> GitResult<Vec<StatusEntry>>;
    fn status_for_path(&self, path: &RepoPath) -> GitResult<Option<StatusEntry>>;

    // ── Branch operations ──────────────────────────────────
    fn current_branch(&self) -> Option<Branch>;
    fn branches(&self) -> GitResult<Vec<Branch>>;
    fn create_branch(&self, name: &str) -> GitResult<()>;
    fn checkout(&self, target: &str, env: &HashMap<String, String>) -> GitResult<()>;
    fn delete_branch(&self, name: &str) -> GitResult<()>;
    fn merge_base(&self, a: &str, b: &str) -> GitResult<Option<String>>;
    fn remote_url(&self, name: &str) -> Option<String>;

    // ── Diff / Content ─────────────────────────────────────
    fn head_text_for_path(&self, path: &RepoPath) -> GitResult<Option<String>>;
    fn index_text_for_path(&self, path: &RepoPath) -> GitResult<Option<String>>;

    // ── Blame ──────────────────────────────────────────────
    fn blame_for_path(
        &self,
        path: &RepoPath,
        content: &str,
    ) -> GitResult<Vec<BlameEntry>>;

    // ── Stash ──────────────────────────────────────────────
    fn stash_list(&self) -> GitResult<Vec<StashEntry>>;
    fn stash_all(&self, message: Option<&str>) -> GitResult<()>;
    fn stash_pop(&self, index: usize) -> GitResult<()>;
    fn stash_apply(&self, index: usize) -> GitResult<()>;
    fn stash_drop(&self, index: usize) -> GitResult<()>;

    // ── History ────────────────────────────────────────────
    fn log(
        &self,
        path: Option<&RepoPath>,
        limit: usize,
    ) -> GitResult<Vec<CommitSummary>>;

    fn show(&self, oid: &str) -> GitResult<CommitDetails>;

    // ── Conflict resolution ────────────────────────────────
    fn checkout_conflict_path(
        &self,
        path: &RepoPath,
        side: ConflictSide,
    ) -> GitResult<()>;
}
