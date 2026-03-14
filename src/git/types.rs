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

// ---------------------------------------------------------------------------
// Remote
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteInfo {
    pub name: String,
    pub url: String,
}

// ---------------------------------------------------------------------------
// Forge types — GitHub-specific entities
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ForgeType {
    GitHub,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum PrState {
    Open,
    Closed,
    Merged,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ReviewDecision {
    Approved,
    ChangesRequested,
    ReviewRequired,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum ChecksStatus {
    Pending,
    Success,
    Failure,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum IssueState {
    Open,
    Closed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RunStatus {
    Queued,
    InProgress,
    Completed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunConclusion {
    Success,
    Failure,
    Cancelled,
    Skipped,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum CheckStatus {
    Queued,
    InProgress,
    Completed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckConclusion {
    Success,
    Failure,
    Neutral,
    Cancelled,
    Skipped,
    TimedOut,
    ActionRequired,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeMethod {
    Merge,
    Squash,
    Rebase,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PullRequest {
    pub number: u32,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub state: Option<PrState>,
    #[serde(default, rename = "headRefName")]
    pub head_branch: String,
    #[serde(default, rename = "baseRefName")]
    pub base_branch: String,
    #[serde(default)]
    pub author: Option<PrAuthor>,
    #[serde(default)]
    pub url: String,
    #[serde(default, rename = "isDraft")]
    pub is_draft: bool,
    #[serde(default, rename = "reviewDecision")]
    pub review_decision: Option<ReviewDecision>,
    #[serde(default)]
    pub additions: u32,
    #[serde(default)]
    pub deletions: u32,
    #[serde(default, rename = "statusCheckRollup")]
    pub checks_status: Option<ChecksStatus>,
    #[serde(default)]
    pub labels: Vec<LabelItem>,
    #[serde(default, rename = "createdAt")]
    pub created_at: String,
    #[serde(default, rename = "updatedAt")]
    pub updated_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PrAuthor {
    pub login: String,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LabelItem {
    pub name: String,
}

impl PullRequest {
    pub fn author_login(&self) -> &str {
        self.author
            .as_ref()
            .map(|a| a.login.as_str())
            .unwrap_or("")
    }

    pub fn label_names(&self) -> Vec<&str> {
        self.labels.iter().map(|l| l.name.as_str()).collect()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Issue {
    pub number: u32,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub state: Option<IssueState>,
    #[serde(default)]
    pub author: Option<PrAuthor>,
    #[serde(default)]
    pub labels: Vec<LabelItem>,
    #[serde(default)]
    pub assignees: Vec<PrAuthor>,
    #[serde(default)]
    pub url: String,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct WorkflowRun {
    #[serde(default, rename = "databaseId")]
    pub id: u64,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub status: Option<RunStatus>,
    #[serde(default)]
    pub conclusion: Option<RunConclusion>,
    #[serde(default, rename = "headBranch")]
    pub head_branch: String,
    #[serde(default)]
    pub event: String,
    #[serde(default)]
    pub url: String,
    #[serde(default, rename = "createdAt")]
    pub created_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CheckRun {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub status: Option<CheckStatus>,
    #[serde(default)]
    pub conclusion: Option<CheckConclusion>,
    #[serde(default, rename = "detailsUrl")]
    pub url: Option<String>,
}

// ---------------------------------------------------------------------------
// Forge operation parameters
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
pub struct PrFilters {
    pub state: Option<PrState>,
    pub head: Option<String>,
    pub base: Option<String>,
    pub limit: Option<u32>,
}

#[derive(Clone, Debug, Default)]
pub struct IssueFilters {
    pub state: Option<IssueState>,
    pub limit: Option<u32>,
}

#[derive(Clone, Debug, Default)]
pub struct RunFilters {
    pub branch: Option<String>,
    pub limit: Option<u32>,
}

#[derive(Clone, Debug)]
pub struct CreatePrOptions {
    pub title: String,
    pub body: String,
    pub base: String,
    pub head: String,
    pub draft: bool,
}

#[derive(Clone, Debug)]
pub struct CreateIssueOptions {
    pub title: String,
    pub body: String,
    pub labels: Vec<String>,
    pub assignees: Vec<String>,
}

#[derive(Clone, Debug)]
pub struct RepoMetadata {
    pub owner: String,
    pub name: String,
    pub default_branch: String,
    pub url: String,
    pub is_fork: bool,
}

// ---------------------------------------------------------------------------
// GitHub integration settings
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct GitHubSettings {
    pub enabled: bool,
    pub poll_interval_secs: u32,
    pub show_workflow_runs: bool,
    pub show_pr_indicator: bool,
    pub gh_binary_path: Option<String>,
}

impl Default for GitHubSettings {
    fn default() -> Self {
        Self {
            enabled: true,
            poll_interval_secs: 60,
            show_workflow_runs: true,
            show_pr_indicator: true,
            gh_binary_path: None,
        }
    }
}

impl GitHubSettings {
    pub fn poll_interval_secs_clamped(&self) -> u32 {
        self.poll_interval_secs.max(15)
    }
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
