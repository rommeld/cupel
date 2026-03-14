use std::path::PathBuf;

use super::types::RepoPath;

#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("git2 error: {0}")]
    Git2(#[from] git2::Error),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("git CLI failed (exit {exit_code}): {stderr}")]
    CliError { exit_code: i32, stderr: String },

    #[error("repository not found at {path}")]
    RepoNotFound { path: PathBuf },

    #[error("path not found in repository: {0}")]
    PathNotFound(RepoPath),

    #[error("invalid operation: {0}")]
    InvalidOperation(String),

    #[error("merge conflict in {path}")]
    MergeConflict { path: RepoPath },

    // ── Forge (gh CLI) errors ─────────────────────────────
    #[error("gh CLI not installed")]
    GhNotInstalled,

    #[error("gh CLI not authenticated — run `gh auth login`")]
    GhNotAuthenticated,

    #[error("remote is not a GitHub repository")]
    GhNotGitHubRepo,

    #[error("GitHub API rate limited — retry after backoff")]
    GhRateLimited,

    #[error("GitHub API error ({status}): {message}")]
    GhApiError { status: u16, message: String },

    #[error("gh CLI failed: {0}")]
    GhError(String),

    #[error("JSON parse error: {0}")]
    JsonError(String),
}

pub type GitResult<T> = Result<T, GitError>;
