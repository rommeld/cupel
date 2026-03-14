use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use crate::git::error::GitResult;
use crate::git::types::{
    BlameEntry, Branch, CheckRun, CommitDetails, CommitOptions, CommitSummary, ConflictSide,
    CreateIssueOptions, CreatePrOptions, ForgeType, Issue, IssueFilters, MergeMethod, PrFilters,
    PullRequest, PushOptions, RepoMetadata, RepoPath, RunFilters, StashEntry, StatusEntry,
    WorkflowRun,
};

// ---------------------------------------------------------------------------
// GitLocalOps — local git operations
// ---------------------------------------------------------------------------

/// All local git operations. Implemented by git2-backed and fake backends.
pub trait GitLocalOps: Send + Sync {
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

// ---------------------------------------------------------------------------
// GitRemoteOps — network git operations
// ---------------------------------------------------------------------------

/// Remote git operations (fetch, pull, push). Primarily git CLI-backed.
pub trait GitRemoteOps: Send + Sync {
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
}

// ---------------------------------------------------------------------------
// GitForgeOps — GitHub-platform operations
// ---------------------------------------------------------------------------

/// GitHub-specific operations via `gh` CLI. Only available when gh is
/// installed, authenticated, and the remote is a GitHub repository.
pub trait GitForgeOps: Send + Sync {
    fn forge_type(&self) -> ForgeType;
    fn is_available(&self) -> bool;

    // ── Pull Requests ──────────────────────────────────────
    fn list_prs(&self, filters: &PrFilters) -> GitResult<Vec<PullRequest>>;
    fn get_pr(&self, number: u32) -> GitResult<PullRequest>;
    fn create_pr(&self, opts: &CreatePrOptions) -> GitResult<PullRequest>;
    fn checkout_pr(&self, number: u32) -> GitResult<()>;
    fn merge_pr(&self, number: u32, method: MergeMethod) -> GitResult<()>;
    fn pr_for_branch(&self, branch: &str) -> GitResult<Option<PullRequest>>;
    fn pr_checks(&self, number: u32) -> GitResult<Vec<CheckRun>>;

    // ── Issues ─────────────────────────────────────────────
    fn list_issues(&self, filters: &IssueFilters) -> GitResult<Vec<Issue>>;
    fn create_issue(&self, opts: &CreateIssueOptions) -> GitResult<Issue>;
    fn develop_issue(&self, number: u32) -> GitResult<String>;

    // ── Workflow Runs ──────────────────────────────────────
    fn list_runs(&self, filters: &RunFilters) -> GitResult<Vec<WorkflowRun>>;
    fn rerun_workflow(&self, run_id: u64) -> GitResult<()>;
    fn run_status(&self, run_id: u64) -> GitResult<WorkflowRun>;

    // ── Repo metadata ──────────────────────────────────────
    fn repo_view(&self) -> GitResult<RepoMetadata>;

    // ── Raw API escape hatch ───────────────────────────────
    fn api_request(
        &self,
        method: &str,
        endpoint: &str,
        body: Option<&str>,
    ) -> GitResult<serde_json::Value>;
}

// ---------------------------------------------------------------------------
// BackendCapabilities
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default)]
pub struct BackendCapabilities {
    pub local_libgit2: bool,
    pub local_git_cli: bool,
    pub remote_git_cli: bool,
    pub remote_libgit2: bool,
    pub forge_github: bool,
}

// ---------------------------------------------------------------------------
// GhAuthStatus — connection indicator state
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GhAuthStatus {
    /// gh CLI not installed — hide all forge UI.
    NotInstalled,
    /// gh CLI installed but not authenticated.
    NotAuthenticated,
    /// Fully connected to GitHub.
    Connected,
}

// ---------------------------------------------------------------------------
// BackendRouter
// ---------------------------------------------------------------------------

/// Dispatches operations to the correct backend.
///
/// Holds separate trait objects for local, remote, and forge operations.
/// For the initial implementation, `local` and `remote` are typically backed
/// by the same `RealGitRepository` instance.
#[derive(Clone)]
pub struct BackendRouter {
    local: Arc<dyn GitLocalOps>,
    remote: Arc<dyn GitRemoteOps>,
    forge: Option<Arc<dyn GitForgeOps>>,
    pub capabilities: BackendCapabilities,
    pub auth_status: GhAuthStatus,
}

impl BackendRouter {
    /// Create a router from a combined local+remote backend (e.g., RealGitRepository).
    pub fn new<T: GitLocalOps + GitRemoteOps + 'static>(
        backend: Arc<T>,
        forge: Option<Arc<dyn GitForgeOps>>,
    ) -> Self {
        let has_forge = forge.as_ref().is_some_and(|f| f.is_available());
        let auth_status = if forge.is_some() {
            if has_forge {
                GhAuthStatus::Connected
            } else {
                GhAuthStatus::NotAuthenticated
            }
        } else {
            GhAuthStatus::NotInstalled
        };

        Self {
            local: backend.clone() as Arc<dyn GitLocalOps>,
            remote: backend as Arc<dyn GitRemoteOps>,
            forge,
            capabilities: BackendCapabilities {
                local_libgit2: true,
                local_git_cli: true,
                remote_git_cli: true,
                remote_libgit2: false,
                forge_github: has_forge,
            },
            auth_status,
        }
    }

    /// Access local git operations.
    pub fn local(&self) -> &dyn GitLocalOps {
        &*self.local
    }

    /// Access remote git operations.
    pub fn remote(&self) -> &dyn GitRemoteOps {
        &*self.remote
    }

    /// Access forge (GitHub) operations. Returns `None` if gh is unavailable.
    pub fn forge(&self) -> Option<&dyn GitForgeOps> {
        self.forge.as_deref()
    }

    /// Whether GitHub forge operations are available.
    pub fn has_forge(&self) -> bool {
        self.capabilities.forge_github
    }
}
