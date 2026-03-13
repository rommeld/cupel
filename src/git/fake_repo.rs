use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::git::error::{GitError, GitResult};
use crate::git::repository::GitRepository;
use crate::git::types::{
    BlameEntry, Branch, CommitDetails, CommitOptions, CommitSummary, ConflictSide, FileStatus,
    GitFileStatus, PushOptions, RepoPath, StashEntry, StatusEntry,
};

/// In-memory `GitRepository` implementation for testing.
///
/// Stores head, index, and working tree state in `HashMap`s and derives
/// status by comparing them. No real git repo or filesystem needed.
pub struct FakeGitRepository {
    head: Mutex<HashMap<RepoPath, String>>,
    index: Mutex<HashMap<RepoPath, String>>,
    worktree: Mutex<HashMap<RepoPath, String>>,
    current_branch: Mutex<Option<String>>,
    branches: Mutex<Vec<Branch>>,
    commits: Mutex<Vec<CommitSummary>>,
    path: PathBuf,
    work_directory: Option<PathBuf>,
}

impl FakeGitRepository {
    pub fn new(path: PathBuf) -> Self {
        let work_dir = Some(path.clone());
        Self {
            head: Mutex::new(HashMap::new()),
            index: Mutex::new(HashMap::new()),
            worktree: Mutex::new(HashMap::new()),
            current_branch: Mutex::new(Some("main".to_string())),
            branches: Mutex::new(vec![Branch {
                name: "main".to_string(),
                upstream: None,
                is_head: true,
                unix_timestamp: None,
            }]),
            commits: Mutex::new(Vec::new()),
            path,
            work_directory: work_dir,
        }
    }

    pub fn set_head_content(&self, path: RepoPath, content: &str) {
        self.head
            .lock()
            .unwrap()
            .insert(path.clone(), content.to_string());
        // Also set in index to match (committed files are in both HEAD and index)
        self.index
            .lock()
            .unwrap()
            .insert(path, content.to_string());
    }

    pub fn set_index_content(&self, path: RepoPath, content: &str) {
        self.index
            .lock()
            .unwrap()
            .insert(path, content.to_string());
    }

    pub fn set_worktree_content(&self, path: RepoPath, content: &str) {
        self.worktree
            .lock()
            .unwrap()
            .insert(path, content.to_string());
    }

    pub fn remove_worktree_content(&self, path: &RepoPath) {
        self.worktree.lock().unwrap().remove(path);
    }

    pub fn set_branch(&self, name: &str) {
        *self.current_branch.lock().unwrap() = Some(name.to_string());
    }

    pub fn add_commit(&self, summary: CommitSummary) {
        self.commits.lock().unwrap().push(summary);
    }

    fn derive_status_for(
        &self,
        path: &RepoPath,
        head: &HashMap<RepoPath, String>,
        index: &HashMap<RepoPath, String>,
        worktree: &HashMap<RepoPath, String>,
    ) -> Option<GitFileStatus> {
        let in_head = head.get(path);
        let in_index = index.get(path);
        let in_worktree = worktree.get(path);

        // Derive index_status by comparing index vs head
        let index_status = match (in_head, in_index) {
            (None, None) => FileStatus::Unchanged,
            (None, Some(_)) => FileStatus::Added,
            (Some(_), None) => FileStatus::Deleted,
            (Some(h), Some(i)) => {
                if h == i {
                    FileStatus::Unchanged
                } else {
                    FileStatus::Modified
                }
            }
        };

        // Derive worktree_status by comparing worktree vs index
        let worktree_status = match (in_index, in_worktree) {
            (None, None) => return None, // not tracked, not in worktree
            (None, Some(_)) => {
                if in_head.is_none() {
                    FileStatus::Untracked
                } else {
                    // Deleted from index but present in worktree
                    FileStatus::Modified
                }
            }
            (Some(_), None) => FileStatus::Deleted,
            (Some(i), Some(w)) => {
                if i == w {
                    FileStatus::Unchanged
                } else {
                    FileStatus::Modified
                }
            }
        };

        // If both unchanged and file exists, it's a clean file — no status entry
        if index_status == FileStatus::Unchanged && worktree_status == FileStatus::Unchanged {
            return None;
        }

        Some(GitFileStatus {
            index_status,
            worktree_status,
            conflict: false,
        })
    }
}

impl GitRepository for FakeGitRepository {
    fn path(&self) -> &Path {
        &self.path
    }

    fn work_directory(&self) -> Option<&Path> {
        self.work_directory.as_deref()
    }

    fn status(&self, _path_prefixes: &[RepoPath]) -> GitResult<Vec<StatusEntry>> {
        let head = self.head.lock().unwrap();
        let index = self.index.lock().unwrap();
        let worktree = self.worktree.lock().unwrap();

        // Collect all known paths
        let mut all_paths: Vec<RepoPath> = head
            .keys()
            .chain(index.keys())
            .chain(worktree.keys())
            .cloned()
            .collect();
        all_paths.sort();
        all_paths.dedup();

        let mut entries = Vec::new();
        for path in all_paths {
            if let Some(status) = self.derive_status_for(&path, &head, &index, &worktree) {
                entries.push(StatusEntry {
                    repo_path: path,
                    status,
                });
            }
        }

        Ok(entries)
    }

    fn status_for_path(&self, path: &RepoPath) -> GitResult<Option<StatusEntry>> {
        let head = self.head.lock().unwrap();
        let index = self.index.lock().unwrap();
        let worktree = self.worktree.lock().unwrap();

        Ok(self
            .derive_status_for(path, &head, &index, &worktree)
            .map(|status| StatusEntry {
                repo_path: path.clone(),
                status,
            }))
    }

    fn stage_paths(
        &self,
        paths: &[RepoPath],
        _env: &HashMap<String, String>,
    ) -> GitResult<()> {
        let mut index = self.index.lock().unwrap();
        let worktree = self.worktree.lock().unwrap();

        for path in paths {
            if let Some(content) = worktree.get(path) {
                index.insert(path.clone(), content.clone());
            } else {
                // File deleted in worktree — remove from index
                index.remove(path);
            }
        }

        Ok(())
    }

    fn unstage_paths(
        &self,
        paths: &[RepoPath],
        _env: &HashMap<String, String>,
    ) -> GitResult<()> {
        let head = self.head.lock().unwrap();
        let mut index = self.index.lock().unwrap();

        for path in paths {
            if let Some(content) = head.get(path) {
                index.insert(path.clone(), content.clone());
            } else {
                index.remove(path);
            }
        }

        Ok(())
    }

    fn set_index_text(
        &self,
        path: &RepoPath,
        content: Option<String>,
        _env: &HashMap<String, String>,
    ) -> GitResult<()> {
        let mut index = self.index.lock().unwrap();
        match content {
            Some(text) => {
                index.insert(path.clone(), text);
            }
            None => {
                index.remove(path);
            }
        }
        Ok(())
    }

    fn reload_index(&self) {
        // No-op for fake
    }

    fn commit(
        &self,
        message: &str,
        _options: &CommitOptions,
        _env: &HashMap<String, String>,
    ) -> GitResult<()> {
        let index = self.index.lock().unwrap();
        let mut head = self.head.lock().unwrap();

        // Copy index state to head
        *head = index.clone();

        // Record commit
        let mut commits = self.commits.lock().unwrap();
        let sha = format!("fake-{}", commits.len());
        commits.push(CommitSummary {
            sha,
            message: message.to_string(),
            author: "Test User".to_string(),
            timestamp: 0,
        });

        Ok(())
    }

    fn uncommit(&self, _env: &HashMap<String, String>) -> GitResult<()> {
        let mut commits = self.commits.lock().unwrap();
        if commits.is_empty() {
            return Err(GitError::InvalidOperation(
                "no commits to undo".to_string(),
            ));
        }
        commits.pop();
        Ok(())
    }

    fn push(
        &self,
        _branch: &str,
        _remote: Option<&str>,
        _options: &PushOptions,
        _env: &HashMap<String, String>,
    ) -> GitResult<()> {
        Ok(())
    }

    fn pull(
        &self,
        _rebase: bool,
        _env: &HashMap<String, String>,
    ) -> GitResult<()> {
        Ok(())
    }

    fn fetch(&self, _env: &HashMap<String, String>) -> GitResult<()> {
        Ok(())
    }

    fn create_remote(&self, _name: &str, _url: &str) -> GitResult<()> {
        Ok(())
    }

    fn current_branch(&self) -> Option<Branch> {
        let name = self.current_branch.lock().unwrap().clone()?;
        Some(Branch {
            name,
            upstream: None,
            is_head: true,
            unix_timestamp: None,
        })
    }

    fn branches(&self) -> GitResult<Vec<Branch>> {
        Ok(self.branches.lock().unwrap().clone())
    }

    fn create_branch(&self, name: &str) -> GitResult<()> {
        self.branches.lock().unwrap().push(Branch {
            name: name.to_string(),
            upstream: None,
            is_head: false,
            unix_timestamp: None,
        });
        Ok(())
    }

    fn checkout(&self, target: &str, _env: &HashMap<String, String>) -> GitResult<()> {
        *self.current_branch.lock().unwrap() = Some(target.to_string());
        Ok(())
    }

    fn delete_branch(&self, name: &str) -> GitResult<()> {
        self.branches
            .lock()
            .unwrap()
            .retain(|b| b.name != name);
        Ok(())
    }

    fn merge_base(&self, _a: &str, _b: &str) -> GitResult<Option<String>> {
        Ok(None)
    }

    fn remote_url(&self, _name: &str) -> Option<String> {
        None
    }

    fn head_text_for_path(&self, path: &RepoPath) -> GitResult<Option<String>> {
        Ok(self.head.lock().unwrap().get(path).cloned())
    }

    fn index_text_for_path(&self, path: &RepoPath) -> GitResult<Option<String>> {
        Ok(self.index.lock().unwrap().get(path).cloned())
    }

    fn blame_for_path(
        &self,
        _path: &RepoPath,
        _content: &str,
    ) -> GitResult<Vec<BlameEntry>> {
        Ok(vec![])
    }

    fn stash_list(&self) -> GitResult<Vec<StashEntry>> {
        Ok(vec![])
    }

    fn stash_all(&self, _message: Option<&str>) -> GitResult<()> {
        Ok(())
    }

    fn stash_pop(&self, _index: usize) -> GitResult<()> {
        Ok(())
    }

    fn stash_apply(&self, _index: usize) -> GitResult<()> {
        Ok(())
    }

    fn stash_drop(&self, _index: usize) -> GitResult<()> {
        Ok(())
    }

    fn log(
        &self,
        _path: Option<&RepoPath>,
        limit: usize,
    ) -> GitResult<Vec<CommitSummary>> {
        let commits = self.commits.lock().unwrap();
        Ok(commits.iter().rev().take(limit).cloned().collect())
    }

    fn show(&self, oid: &str) -> GitResult<CommitDetails> {
        let commits = self.commits.lock().unwrap();
        let summary = commits
            .iter()
            .find(|c| c.sha == oid)
            .cloned()
            .ok_or_else(|| {
                GitError::InvalidOperation(format!("commit not found: {oid}"))
            })?;
        Ok(CommitDetails {
            summary,
            parent_shas: vec![],
            diff_stats: None,
        })
    }

    fn checkout_conflict_path(
        &self,
        _path: &RepoPath,
        _side: ConflictSide,
    ) -> GitResult<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_fake() -> FakeGitRepository {
        FakeGitRepository::new(PathBuf::from("/fake/repo"))
    }

    #[test]
    fn test_empty_status() {
        let repo = make_fake();
        let status = repo.status(&[]).unwrap();
        assert!(status.is_empty());
    }

    #[test]
    fn test_untracked_file() {
        let repo = make_fake();
        repo.set_worktree_content(RepoPath::from("new.txt"), "hello");

        let status = repo.status(&[]).unwrap();
        assert_eq!(status.len(), 1);
        assert_eq!(status[0].status.worktree_status, FileStatus::Untracked);
    }

    #[test]
    fn test_modified_file() {
        let repo = make_fake();
        repo.set_head_content(RepoPath::from("file.rs"), "original");
        repo.set_worktree_content(RepoPath::from("file.rs"), "modified");

        let status = repo.status(&[]).unwrap();
        assert_eq!(status.len(), 1);
        assert_eq!(status[0].status.worktree_status, FileStatus::Modified);
        assert_eq!(status[0].status.index_status, FileStatus::Unchanged);
    }

    #[test]
    fn test_staged_file() {
        let repo = make_fake();
        repo.set_head_content(RepoPath::from("file.rs"), "original");
        repo.set_worktree_content(RepoPath::from("file.rs"), "modified");

        repo.stage_paths(&[RepoPath::from("file.rs")], &HashMap::new())
            .unwrap();

        let status = repo.status(&[]).unwrap();
        assert_eq!(status.len(), 1);
        assert_eq!(status[0].status.index_status, FileStatus::Modified);
        assert_eq!(status[0].status.worktree_status, FileStatus::Unchanged);
    }

    #[test]
    fn test_commit_clears_status() {
        let repo = make_fake();
        repo.set_head_content(RepoPath::from("file.rs"), "original");
        repo.set_worktree_content(RepoPath::from("file.rs"), "modified");
        repo.stage_paths(&[RepoPath::from("file.rs")], &HashMap::new())
            .unwrap();

        repo.commit("test commit", &CommitOptions::default(), &HashMap::new())
            .unwrap();

        let status = repo.status(&[]).unwrap();
        // After commit, HEAD matches index matches worktree → clean
        assert!(
            status.is_empty(),
            "expected clean status after commit, got: {status:?}"
        );
    }

    #[test]
    fn test_set_index_text_roundtrip() {
        let repo = make_fake();
        repo.set_index_text(
            &RepoPath::from("file.txt"),
            Some("custom content".to_string()),
            &HashMap::new(),
        )
        .unwrap();

        let text = repo
            .index_text_for_path(&RepoPath::from("file.txt"))
            .unwrap();
        assert_eq!(text, Some("custom content".to_string()));
    }

    #[test]
    fn test_head_text_roundtrip() {
        let repo = make_fake();
        repo.set_head_content(RepoPath::from("main.rs"), "fn main() {}");

        let text = repo
            .head_text_for_path(&RepoPath::from("main.rs"))
            .unwrap();
        assert_eq!(text, Some("fn main() {}".to_string()));
    }

    #[test]
    fn test_unstage_restores_from_head() {
        let repo = make_fake();
        repo.set_head_content(RepoPath::from("file.rs"), "original");
        repo.set_worktree_content(RepoPath::from("file.rs"), "modified");
        repo.stage_paths(&[RepoPath::from("file.rs")], &HashMap::new())
            .unwrap();

        repo.unstage_paths(&[RepoPath::from("file.rs")], &HashMap::new())
            .unwrap();

        let status = repo.status(&[]).unwrap();
        assert_eq!(status.len(), 1);
        assert_eq!(status[0].status.index_status, FileStatus::Unchanged);
        assert_eq!(status[0].status.worktree_status, FileStatus::Modified);
    }

    #[test]
    fn test_deleted_in_worktree() {
        let repo = make_fake();
        repo.set_head_content(RepoPath::from("file.rs"), "content");
        // File is in HEAD and index but not in worktree

        let status = repo.status(&[]).unwrap();
        assert_eq!(status.len(), 1);
        assert_eq!(status[0].status.worktree_status, FileStatus::Deleted);
    }

    #[test]
    fn test_log_returns_most_recent_first() {
        let repo = make_fake();
        repo.add_commit(CommitSummary {
            sha: "aaa".to_string(),
            message: "first".to_string(),
            author: "test".to_string(),
            timestamp: 1,
        });
        repo.add_commit(CommitSummary {
            sha: "bbb".to_string(),
            message: "second".to_string(),
            author: "test".to_string(),
            timestamp: 2,
        });

        let log = repo.log(None, 10).unwrap();
        assert_eq!(log.len(), 2);
        assert_eq!(log[0].message, "second");
        assert_eq!(log[1].message, "first");
    }

    #[test]
    fn test_branch_operations() {
        let repo = make_fake();
        assert_eq!(
            repo.current_branch().unwrap().name,
            "main"
        );

        repo.create_branch("feature").unwrap();
        let branches = repo.branches().unwrap();
        assert_eq!(branches.len(), 2);

        repo.checkout("feature", &HashMap::new()).unwrap();
        assert_eq!(
            repo.current_branch().unwrap().name,
            "feature"
        );

        repo.delete_branch("feature").unwrap();
        let branches = repo.branches().unwrap();
        assert_eq!(branches.len(), 1);
    }
}
