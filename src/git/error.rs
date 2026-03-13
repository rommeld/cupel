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
}

pub type GitResult<T> = Result<T, GitError>;
