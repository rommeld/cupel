use std::fmt;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

// ---------------------------------------------------------------------------
// RepoPath — a path relative to the repository root
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RepoPath(Arc<Path>);

impl RepoPath {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self(Arc::from(path.into()))
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

impl PartialOrd for RepoPath {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for RepoPath {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.cmp(&other.0)
    }
}

impl fmt::Display for RepoPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0.display())
    }
}

impl From<&str> for RepoPath {
    fn from(s: &str) -> Self {
        Self::new(PathBuf::from(s))
    }
}

impl From<PathBuf> for RepoPath {
    fn from(p: PathBuf) -> Self {
        Self::new(p)
    }
}

impl From<&Path> for RepoPath {
    fn from(p: &Path) -> Self {
        Self::new(p.to_path_buf())
    }
}

impl AsRef<Path> for RepoPath {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

// ---------------------------------------------------------------------------
// File & staging status
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FileStatus {
    Added,
    Modified,
    Deleted,
    Untracked,
    Unchanged,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GitFileStatus {
    pub index_status: FileStatus,
    pub worktree_status: FileStatus,
    pub conflict: bool,
}

impl GitFileStatus {
    pub fn staging_state(&self) -> StagingState {
        match (self.index_status, self.worktree_status) {
            (FileStatus::Unchanged, _) => StagingState::Unstaged,
            (_, FileStatus::Unchanged) => StagingState::Staged,
            _ => StagingState::PartiallyStaged,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StagingState {
    Staged,
    Unstaged,
    PartiallyStaged,
}

// ---------------------------------------------------------------------------
// StatusEntry
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusEntry {
    pub repo_path: RepoPath,
    pub status: GitFileStatus,
}

impl StatusEntry {
    pub fn staging_state(&self) -> StagingState {
        self.status.staging_state()
    }
}

// ---------------------------------------------------------------------------
// Branch
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Branch {
    pub name: String,
    pub upstream: Option<UpstreamBranch>,
    pub is_head: bool,
    pub unix_timestamp: Option<i64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpstreamBranch {
    pub name: String,
    pub ahead: Option<u32>,
    pub behind: Option<u32>,
}

// ---------------------------------------------------------------------------
// Blame
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlameEntry {
    pub sha: String,
    pub line_range: Range<u32>,
    pub author: Option<String>,
    pub author_mail: Option<String>,
    pub author_timestamp: Option<i64>,
    pub committer: Option<String>,
    pub summary: Option<String>,
}

// ---------------------------------------------------------------------------
// Commit
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
pub struct CommitOptions {
    pub amend: bool,
    pub signoff: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitSummary {
    pub sha: String,
    pub message: String,
    pub author: String,
    pub timestamp: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitDetails {
    pub summary: CommitSummary,
    pub parent_shas: Vec<String>,
    pub diff_stats: Option<String>,
}

// ---------------------------------------------------------------------------
// Push
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
pub struct PushOptions {
    pub force: bool,
    pub set_upstream: bool,
}

// ---------------------------------------------------------------------------
// Stash
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StashEntry {
    pub index: usize,
    pub message: String,
    pub sha: String,
}

// ---------------------------------------------------------------------------
// Conflict
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConflictSide {
    Ours,
    Theirs,
    Base,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_path_ordering() {
        let a = RepoPath::from("a/b.rs");
        let b = RepoPath::from("a/c.rs");
        let c = RepoPath::from("b/a.rs");
        assert!(a < b);
        assert!(b < c);
        assert!(a < c);
    }

    #[test]
    fn repo_path_display_roundtrip() {
        let path = RepoPath::from("src/main.rs");
        assert_eq!(path.to_string(), "src/main.rs");
    }

    #[test]
    fn repo_path_equality() {
        let a = RepoPath::from("src/lib.rs");
        let b = RepoPath::from("src/lib.rs");
        assert_eq!(a, b);
    }

    #[test]
    fn staging_state_fully_unstaged() {
        let status = GitFileStatus {
            index_status: FileStatus::Unchanged,
            worktree_status: FileStatus::Modified,
            conflict: false,
        };
        assert_eq!(status.staging_state(), StagingState::Unstaged);
    }

    #[test]
    fn staging_state_fully_staged() {
        let status = GitFileStatus {
            index_status: FileStatus::Modified,
            worktree_status: FileStatus::Unchanged,
            conflict: false,
        };
        assert_eq!(status.staging_state(), StagingState::Staged);
    }

    #[test]
    fn staging_state_partially_staged() {
        let status = GitFileStatus {
            index_status: FileStatus::Modified,
            worktree_status: FileStatus::Modified,
            conflict: false,
        };
        assert_eq!(status.staging_state(), StagingState::PartiallyStaged);
    }

    #[test]
    fn staging_state_both_unchanged() {
        let status = GitFileStatus {
            index_status: FileStatus::Unchanged,
            worktree_status: FileStatus::Unchanged,
            conflict: false,
        };
        assert_eq!(status.staging_state(), StagingState::Unstaged);
    }
}
