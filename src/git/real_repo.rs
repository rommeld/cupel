use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use crate::git::backend_router::{GitLocalOps, GitRemoteOps};
use crate::git::error::{GitError, GitResult};
use crate::git::types::{
    BlameEntry, Branch, CommitDetails, CommitOptions, CommitSummary, ConflictSide, FileStatus,
    GitFileStatus, PushOptions, RepoPath, StashEntry, StatusEntry,
};

/// Git repository backed by git2 for local operations and the git CLI for
/// network operations.
pub struct RealGitRepository {
    repository: Mutex<git2::Repository>,
    path: PathBuf,
    work_directory: Option<PathBuf>,
}

impl RealGitRepository {
    pub fn open(path: &Path) -> GitResult<Self> {
        let repo = git2::Repository::open(path)?;
        let git_path = repo.path().to_path_buf();
        let work_dir = repo.workdir().map(|p| p.to_path_buf());
        Ok(Self {
            repository: Mutex::new(repo),
            path: git_path,
            work_directory: work_dir,
        })
    }

    pub fn discover(path: &Path) -> GitResult<Self> {
        let repo = git2::Repository::discover(path)?;
        let git_path = repo.path().to_path_buf();
        let work_dir = repo.workdir().map(|p| p.to_path_buf());
        Ok(Self {
            repository: Mutex::new(repo),
            path: git_path,
            work_directory: work_dir,
        })
    }

    fn run_git(
        &self,
        args: &[&str],
        env: &HashMap<String, String>,
    ) -> GitResult<String> {
        let cwd = self
            .work_directory
            .as_deref()
            .unwrap_or(&self.path);

        let output = Command::new("git")
            .args(args)
            .envs(env)
            .current_dir(cwd)
            .output()?;

        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        } else {
            Err(GitError::CliError {
                exit_code: output.status.code().unwrap_or(-1),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            })
        }
    }

    fn map_git2_status(status: git2::Status) -> GitFileStatus {
        let index_status = if status.contains(git2::Status::INDEX_NEW) {
            FileStatus::Added
        } else if status.contains(git2::Status::INDEX_MODIFIED)
            || status.contains(git2::Status::INDEX_RENAMED)
            || status.contains(git2::Status::INDEX_TYPECHANGE)
        {
            FileStatus::Modified
        } else if status.contains(git2::Status::INDEX_DELETED) {
            FileStatus::Deleted
        } else {
            FileStatus::Unchanged
        };

        let worktree_status = if status.contains(git2::Status::WT_NEW) {
            FileStatus::Untracked
        } else if status.contains(git2::Status::WT_MODIFIED)
            || status.contains(git2::Status::WT_RENAMED)
            || status.contains(git2::Status::WT_TYPECHANGE)
        {
            FileStatus::Modified
        } else if status.contains(git2::Status::WT_DELETED) {
            FileStatus::Deleted
        } else {
            FileStatus::Unchanged
        };

        let conflict = status.contains(git2::Status::CONFLICTED);

        GitFileStatus {
            index_status,
            worktree_status,
            conflict,
        }
    }
}

// ---------------------------------------------------------------------------
// GitLocalOps implementation
// ---------------------------------------------------------------------------

impl GitLocalOps for RealGitRepository {
    fn path(&self) -> &Path {
        &self.path
    }

    fn work_directory(&self) -> Option<&Path> {
        self.work_directory.as_deref()
    }

    fn status(&self, path_prefixes: &[RepoPath]) -> GitResult<Vec<StatusEntry>> {
        let repo = self.repository.lock().unwrap();
        let mut opts = git2::StatusOptions::new();
        opts.include_untracked(true)
            .recurse_untracked_dirs(true)
            .include_unmodified(false);

        for prefix in path_prefixes {
            opts.pathspec(prefix.as_path());
        }

        let statuses = repo.statuses(Some(&mut opts))?;
        let mut entries = Vec::with_capacity(statuses.len());

        for entry in statuses.iter() {
            if let Some(path) = entry.path() {
                let status = Self::map_git2_status(entry.status());
                entries.push(StatusEntry {
                    repo_path: RepoPath::from(path),
                    status,
                });
            }
        }

        entries.sort_by(|a, b| a.repo_path.cmp(&b.repo_path));
        Ok(entries)
    }

    fn status_for_path(&self, path: &RepoPath) -> GitResult<Option<StatusEntry>> {
        let repo = self.repository.lock().unwrap();
        match repo.status_file(path.as_path()) {
            Ok(status) => {
                if status.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(StatusEntry {
                        repo_path: path.clone(),
                        status: Self::map_git2_status(status),
                    }))
                }
            }
            Err(e) if e.code() == git2::ErrorCode::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn current_branch(&self) -> Option<Branch> {
        let repo = self.repository.lock().unwrap();
        let head = repo.head().ok()?;
        let name = head.shorthand()?.to_string();

        let upstream = head
            .name()
            .and_then(|refname| repo.find_branch(refname.strip_prefix("refs/heads/")?, git2::BranchType::Local).ok())
            .and_then(|branch| {
                let upstream = branch.upstream().ok()?;
                let upstream_name = upstream.name().ok()??.to_string();
                Some(crate::git::types::UpstreamBranch {
                    name: upstream_name,
                    ahead: None,
                    behind: None,
                })
            });

        Some(Branch {
            name,
            upstream,
            is_head: true,
            unix_timestamp: None,
        })
    }

    fn branches(&self) -> GitResult<Vec<Branch>> {
        let repo = self.repository.lock().unwrap();
        let branches = repo.branches(None)?;
        let mut result = Vec::new();

        for branch_result in branches {
            let (branch, _branch_type) = branch_result?;
            if let Some(name) = branch.name()? {
                let is_head = branch.is_head();
                result.push(Branch {
                    name: name.to_string(),
                    upstream: None,
                    is_head,
                    unix_timestamp: None,
                });
            }
        }

        Ok(result)
    }

    fn merge_base(&self, a: &str, b: &str) -> GitResult<Option<String>> {
        let repo = self.repository.lock().unwrap();
        let oid_a = match repo.revparse_single(a) {
            Ok(obj) => obj.id(),
            Err(_) => return Ok(None),
        };
        let oid_b = match repo.revparse_single(b) {
            Ok(obj) => obj.id(),
            Err(_) => return Ok(None),
        };
        match repo.merge_base(oid_a, oid_b) {
            Ok(oid) => Ok(Some(oid.to_string())),
            Err(_) => Ok(None),
        }
    }

    fn remote_url(&self, name: &str) -> Option<String> {
        let repo = self.repository.lock().unwrap();
        repo.find_remote(name)
            .ok()
            .and_then(|r| r.url().map(|s| s.to_string()))
    }

    fn head_text_for_path(&self, path: &RepoPath) -> GitResult<Option<String>> {
        let repo = self.repository.lock().unwrap();
        let head = match repo.head() {
            Ok(h) => h,
            Err(_) => return Ok(None),
        };
        let tree = head.peel_to_tree()?;
        let entry = match tree.get_path(path.as_path()) {
            Ok(e) => e,
            Err(_) => return Ok(None),
        };
        let blob = repo.find_blob(entry.id())?;
        match std::str::from_utf8(blob.content()) {
            Ok(text) => Ok(Some(text.to_string())),
            Err(_) => Ok(None),
        }
    }

    fn index_text_for_path(&self, path: &RepoPath) -> GitResult<Option<String>> {
        let repo = self.repository.lock().unwrap();
        let index = repo.index()?;
        let entry = match index.get_path(path.as_path(), 0) {
            Some(e) => e,
            None => return Ok(None),
        };
        let blob = repo.find_blob(entry.id)?;
        match std::str::from_utf8(blob.content()) {
            Ok(text) => Ok(Some(text.to_string())),
            Err(_) => Ok(None),
        }
    }

    fn blame_for_path(
        &self,
        path: &RepoPath,
        _content: &str,
    ) -> GitResult<Vec<BlameEntry>> {
        let repo = self.repository.lock().unwrap();
        let blame = repo.blame_file(path.as_path(), None)?;
        let mut entries = Vec::new();

        for i in 0..blame.len() {
            if let Some(hunk) = blame.get_index(i) {
                let sig = hunk.final_signature();
                entries.push(BlameEntry {
                    sha: hunk.final_commit_id().to_string(),
                    line_range: (hunk.final_start_line() as u32)
                        ..(hunk.final_start_line() as u32 + hunk.lines_in_hunk() as u32),
                    author: sig.name().map(|s| s.to_string()),
                    author_mail: sig.email().map(|s| s.to_string()),
                    author_timestamp: Some(sig.when().seconds()),
                    committer: None,
                    summary: None,
                });
            }
        }

        Ok(entries)
    }

    fn stash_list(&self) -> GitResult<Vec<StashEntry>> {
        let mut repo = self.repository.lock().unwrap();
        let mut entries = Vec::new();
        repo.stash_foreach(|index, message, oid| {
            entries.push(StashEntry {
                index,
                message: message.to_string(),
                sha: oid.to_string(),
            });
            true
        })?;
        Ok(entries)
    }

    fn log(
        &self,
        path: Option<&RepoPath>,
        limit: usize,
    ) -> GitResult<Vec<CommitSummary>> {
        let repo = self.repository.lock().unwrap();
        let mut revwalk = repo.revwalk()?;
        revwalk.set_sorting(git2::Sort::TIME)?;
        revwalk.push_head()?;

        let mut results = Vec::new();

        for oid_result in revwalk {
            if results.len() >= limit {
                break;
            }
            let oid = oid_result?;
            let commit = repo.find_commit(oid)?;

            if let Some(filter_path) = path {
                let dominated = commit
                    .tree()
                    .ok()
                    .and_then(|t| t.get_path(filter_path.as_path()).ok())
                    .is_some();
                if !dominated {
                    continue;
                }
            }

            let author = commit.author();
            results.push(CommitSummary {
                sha: oid.to_string(),
                message: commit.message().unwrap_or("").to_string(),
                author: author.name().unwrap_or("").to_string(),
                timestamp: author.when().seconds(),
            });
        }

        Ok(results)
    }

    fn show(&self, oid: &str) -> GitResult<CommitDetails> {
        let repo = self.repository.lock().unwrap();
        let obj = repo.revparse_single(oid)?;
        let commit = obj.peel_to_commit()?;
        let author = commit.author();

        let summary = CommitSummary {
            sha: commit.id().to_string(),
            message: commit.message().unwrap_or("").to_string(),
            author: author.name().unwrap_or("").to_string(),
            timestamp: author.when().seconds(),
        };

        let parent_shas = commit
            .parent_ids()
            .map(|id| id.to_string())
            .collect();

        Ok(CommitDetails {
            summary,
            parent_shas,
            diff_stats: None,
        })
    }

    fn reload_index(&self) {
        let repo = self.repository.lock().unwrap();
        if let Ok(mut index) = repo.index() {
            let _ = index.read(true);
        }
    }

    fn stage_paths(
        &self,
        paths: &[RepoPath],
        env: &HashMap<String, String>,
    ) -> GitResult<()> {
        if paths.is_empty() {
            return Ok(());
        }
        let mut args = vec!["add", "--"];
        let path_strings: Vec<String> = paths
            .iter()
            .map(|p| p.as_path().to_string_lossy().into_owned())
            .collect();
        for s in &path_strings {
            args.push(s);
        }
        self.run_git(&args, env)?;
        Ok(())
    }

    fn unstage_paths(
        &self,
        paths: &[RepoPath],
        env: &HashMap<String, String>,
    ) -> GitResult<()> {
        if paths.is_empty() {
            return Ok(());
        }
        let mut args = vec!["reset", "HEAD", "--"];
        let path_strings: Vec<String> = paths
            .iter()
            .map(|p| p.as_path().to_string_lossy().into_owned())
            .collect();
        for s in &path_strings {
            args.push(s);
        }
        self.run_git(&args, env)?;
        Ok(())
    }

    fn set_index_text(
        &self,
        path: &RepoPath,
        content: Option<String>,
        _env: &HashMap<String, String>,
    ) -> GitResult<()> {
        let repo = self.repository.lock().unwrap();
        let mut index = repo.index()?;

        match content {
            Some(text) => {
                let oid = repo.blob(text.as_bytes())?;
                let entry = git2::IndexEntry {
                    ctime: git2::IndexTime::new(0, 0),
                    mtime: git2::IndexTime::new(0, 0),
                    dev: 0,
                    ino: 0,
                    mode: 0o100644,
                    uid: 0,
                    gid: 0,
                    file_size: text.len() as u32,
                    id: oid,
                    flags: 0,
                    flags_extended: 0,
                    path: path
                        .as_path()
                        .to_string_lossy()
                        .as_bytes()
                        .to_vec(),
                };
                index.add(&entry)?;
            }
            None => {
                index.remove_path(path.as_path())?;
            }
        }

        index.write()?;
        Ok(())
    }

    fn commit(
        &self,
        message: &str,
        options: &CommitOptions,
        env: &HashMap<String, String>,
    ) -> GitResult<()> {
        if options.amend {
            let mut args = vec!["commit", "--amend", "-m", message];
            if options.signoff {
                args.push("--signoff");
            }
            self.run_git(&args, env)?;
            return Ok(());
        }

        let repo = self.repository.lock().unwrap();
        let mut index = repo.index()?;
        let tree_oid = index.write_tree()?;
        let tree = repo.find_tree(tree_oid)?;
        let sig = repo.signature()?;

        let parents = match repo.head() {
            Ok(head) => {
                let parent = head.peel_to_commit()?;
                vec![parent]
            }
            Err(_) => vec![],
        };
        let parent_refs: Vec<&git2::Commit> = parents.iter().collect();

        let mut msg = message.to_string();
        if options.signoff {
            msg.push_str(&format!(
                "\n\nSigned-off-by: {} <{}>",
                sig.name().unwrap_or(""),
                sig.email().unwrap_or("")
            ));
        }

        repo.commit(Some("HEAD"), &sig, &sig, &msg, &tree, &parent_refs)?;
        Ok(())
    }

    fn uncommit(&self, env: &HashMap<String, String>) -> GitResult<()> {
        self.run_git(&["reset", "HEAD^", "--soft"], env)?;
        Ok(())
    }

    fn create_branch(&self, name: &str) -> GitResult<()> {
        let repo = self.repository.lock().unwrap();
        let head = repo.head()?;
        let commit = head.peel_to_commit()?;
        repo.branch(name, &commit, false)?;
        Ok(())
    }

    fn checkout(&self, target: &str, env: &HashMap<String, String>) -> GitResult<()> {
        self.run_git(&["checkout", target], env)?;
        Ok(())
    }

    fn delete_branch(&self, name: &str) -> GitResult<()> {
        let repo = self.repository.lock().unwrap();
        let mut branch = repo.find_branch(name, git2::BranchType::Local)?;
        branch.delete()?;
        Ok(())
    }

    fn stash_all(&self, message: Option<&str>) -> GitResult<()> {
        let mut args = vec!["stash", "push"];
        if let Some(msg) = message {
            args.push("-m");
            args.push(msg);
        }
        self.run_git(&args, &HashMap::new())?;
        Ok(())
    }

    fn stash_pop(&self, index: usize) -> GitResult<()> {
        let stash_ref = format!("stash@{{{index}}}");
        self.run_git(&["stash", "pop", &stash_ref], &HashMap::new())?;
        Ok(())
    }

    fn stash_apply(&self, index: usize) -> GitResult<()> {
        let stash_ref = format!("stash@{{{index}}}");
        self.run_git(&["stash", "apply", &stash_ref], &HashMap::new())?;
        Ok(())
    }

    fn stash_drop(&self, index: usize) -> GitResult<()> {
        let stash_ref = format!("stash@{{{index}}}");
        self.run_git(&["stash", "drop", &stash_ref], &HashMap::new())?;
        Ok(())
    }

    fn checkout_conflict_path(
        &self,
        path: &RepoPath,
        side: ConflictSide,
    ) -> GitResult<()> {
        let side_flag = match side {
            ConflictSide::Ours => "--ours",
            ConflictSide::Theirs => "--theirs",
            ConflictSide::Base => {
                return Err(GitError::InvalidOperation(
                    "cannot checkout base side via CLI".to_string(),
                ));
            }
        };
        let path_str = path.as_path().to_string_lossy().to_string();
        self.run_git(
            &["checkout", side_flag, "--", &path_str],
            &HashMap::new(),
        )?;
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// GitRemoteOps implementation
// ---------------------------------------------------------------------------

impl GitRemoteOps for RealGitRepository {
    fn push(
        &self,
        branch: &str,
        remote: Option<&str>,
        options: &PushOptions,
        env: &HashMap<String, String>,
    ) -> GitResult<()> {
        let remote_name = remote.unwrap_or("origin");
        let mut args = vec!["push", remote_name, branch];
        if options.force {
            args.push("--force");
        }
        if options.set_upstream {
            args.push("--set-upstream");
        }
        self.run_git(&args, env)?;
        Ok(())
    }

    fn pull(
        &self,
        rebase: bool,
        env: &HashMap<String, String>,
    ) -> GitResult<()> {
        let mut args = vec!["pull"];
        if rebase {
            args.push("--rebase");
        }
        self.run_git(&args, env)?;
        Ok(())
    }

    fn fetch(&self, env: &HashMap<String, String>) -> GitResult<()> {
        self.run_git(&["fetch"], env)?;
        Ok(())
    }

    fn create_remote(&self, name: &str, url: &str) -> GitResult<()> {
        let repo = self.repository.lock().unwrap();
        repo.remote(name, url)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn init_test_repo() -> (tempfile::TempDir, RealGitRepository) {
        let dir = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(dir.path()).unwrap();

        // Configure user for commits
        let mut config = repo.config().unwrap();
        config.set_str("user.name", "Test User").unwrap();
        config.set_str("user.email", "test@example.com").unwrap();

        drop(config);
        drop(repo);

        let real_repo = RealGitRepository::open(dir.path()).unwrap();
        (dir, real_repo)
    }

    fn create_initial_commit(dir: &Path) {
        let repo = git2::Repository::open(dir).unwrap();
        let sig = repo.signature().unwrap();
        let mut index = repo.index().unwrap();

        fs::write(dir.join("README.md"), "# Test\n").unwrap();
        index.add_path(Path::new("README.md")).unwrap();
        index.write().unwrap();

        let tree_oid = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_oid).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "initial commit", &tree, &[])
            .unwrap();
    }

    #[test]
    fn test_open_and_identity() {
        let (dir, repo) = init_test_repo();
        assert!(repo.work_directory().is_some());
        let expected = dir.path().canonicalize().unwrap();
        let actual = repo.work_directory().unwrap().canonicalize().unwrap();
        assert_eq!(actual, expected);
    }

    #[test]
    fn test_status_empty_repo() {
        let (_dir, repo) = init_test_repo();
        let status = repo.status(&[]).unwrap();
        assert!(status.is_empty());
    }

    #[test]
    fn test_status_with_untracked_file() {
        let (dir, repo) = init_test_repo();
        fs::write(dir.path().join("new_file.txt"), "hello").unwrap();

        let status = repo.status(&[]).unwrap();
        assert_eq!(status.len(), 1);
        assert_eq!(status[0].repo_path, RepoPath::from("new_file.txt"));
        assert_eq!(status[0].status.worktree_status, FileStatus::Untracked);
    }

    #[test]
    fn test_status_with_staged_file() {
        let (dir, repo) = init_test_repo();
        create_initial_commit(dir.path());

        fs::write(dir.path().join("staged.txt"), "staged content").unwrap();
        repo.stage_paths(
            &[RepoPath::from("staged.txt")],
            &HashMap::new(),
        )
        .unwrap();

        let status = repo.status(&[]).unwrap();
        assert_eq!(status.len(), 1);
        assert_eq!(status[0].status.index_status, FileStatus::Added);
    }

    #[test]
    fn test_current_branch_on_fresh_repo() {
        let (dir, repo) = init_test_repo();
        create_initial_commit(dir.path());

        let branch = repo.current_branch();
        assert!(branch.is_some());
        let branch = branch.unwrap();
        assert!(branch.is_head);
    }

    #[test]
    fn test_head_text_for_path() {
        let (dir, repo) = init_test_repo();
        create_initial_commit(dir.path());

        let text = repo
            .head_text_for_path(&RepoPath::from("README.md"))
            .unwrap();
        assert_eq!(text, Some("# Test\n".to_string()));
    }

    #[test]
    fn test_head_text_missing_path() {
        let (dir, repo) = init_test_repo();
        create_initial_commit(dir.path());

        let text = repo
            .head_text_for_path(&RepoPath::from("nonexistent.txt"))
            .unwrap();
        assert_eq!(text, None);
    }

    #[test]
    fn test_log() {
        let (dir, repo) = init_test_repo();
        create_initial_commit(dir.path());

        let log = repo.log(None, 10).unwrap();
        assert_eq!(log.len(), 1);
        assert_eq!(log[0].message, "initial commit");
        assert_eq!(log[0].author, "Test User");
    }

    #[test]
    fn test_commit_and_log() {
        let (dir, repo) = init_test_repo();
        create_initial_commit(dir.path());

        fs::write(dir.path().join("new.txt"), "new content").unwrap();
        repo.stage_paths(&[RepoPath::from("new.txt")], &HashMap::new())
            .unwrap();
        repo.commit("second commit", &CommitOptions::default(), &HashMap::new())
            .unwrap();

        let log = repo.log(None, 10).unwrap();
        assert_eq!(log.len(), 2);
        assert_eq!(log[0].message, "second commit");
    }

    #[test]
    fn test_set_index_text() {
        let (dir, repo) = init_test_repo();
        create_initial_commit(dir.path());

        repo.set_index_text(
            &RepoPath::from("virtual.txt"),
            Some("virtual content".to_string()),
            &HashMap::new(),
        )
        .unwrap();

        let text = repo
            .index_text_for_path(&RepoPath::from("virtual.txt"))
            .unwrap();
        assert_eq!(text, Some("virtual content".to_string()));
    }

    #[test]
    fn test_set_index_text_remove() {
        let (dir, repo) = init_test_repo();
        create_initial_commit(dir.path());

        repo.set_index_text(
            &RepoPath::from("README.md"),
            None,
            &HashMap::new(),
        )
        .unwrap();

        let text = repo
            .index_text_for_path(&RepoPath::from("README.md"))
            .unwrap();
        assert_eq!(text, None);
    }

    #[test]
    fn test_branches() {
        let (dir, repo) = init_test_repo();
        create_initial_commit(dir.path());

        repo.create_branch("feature-1").unwrap();
        let branches = repo.branches().unwrap();
        assert!(branches.len() >= 2);
        assert!(branches.iter().any(|b| b.name == "feature-1"));
    }

    #[test]
    fn test_stash_list_empty() {
        let (dir, repo) = init_test_repo();
        create_initial_commit(dir.path());

        let stashes = repo.stash_list().unwrap();
        assert!(stashes.is_empty());
    }
}
