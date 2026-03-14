use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;

use crate::git::backend_router::GitForgeOps;
use crate::git::error::{GitError, GitResult};
use crate::git::types::{
    CheckRun, CreateIssueOptions, CreatePrOptions, ForgeType, Issue, IssueFilters, MergeMethod,
    PrFilters, PullRequest, RepoMetadata, RunFilters, WorkflowRun,
};

// ---------------------------------------------------------------------------
// GhCliProcess — low-level process runner
// ---------------------------------------------------------------------------

struct GhCliProcess {
    binary: String,
    work_dir: PathBuf,
}

impl GhCliProcess {
    fn run(&self, args: &[&str]) -> GitResult<String> {
        let output = Command::new(&self.binary)
            .args(args)
            .current_dir(&self.work_dir)
            .output()
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    GitError::GhNotInstalled
                } else {
                    GitError::Io(e)
                }
            })?;

        if output.status.success() {
            Ok(String::from_utf8_lossy(&output.stdout).into_owned())
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
            let exit_code = output.status.code().unwrap_or(-1);

            // Detect specific error conditions
            if stderr.contains("auth login") || stderr.contains("not logged") {
                return Err(GitError::GhNotAuthenticated);
            }
            if stderr.contains("HTTP 429") || stderr.contains("rate limit") {
                return Err(GitError::GhRateLimited);
            }
            if stderr.contains("not a git repository")
                || stderr.contains("not a GitHub repository")
            {
                return Err(GitError::GhNotGitHubRepo);
            }

            Err(GitError::GhError(format!(
                "gh exited with code {exit_code}: {stderr}"
            )))
        }
    }

    fn run_json<T: serde::de::DeserializeOwned>(&self, args: &[&str]) -> GitResult<T> {
        let output = self.run(args)?;
        serde_json::from_str(&output).map_err(|e| {
            GitError::JsonError(format!("{e}: {}", output.chars().take(200).collect::<String>()))
        })
    }
}

// ---------------------------------------------------------------------------
// GhCliBacked — GitForgeOps implementation
// ---------------------------------------------------------------------------

/// GitHub CLI backend implementing `GitForgeOps`.
///
/// Wraps the `gh` CLI binary and uses `--json` structured output for all
/// queries. Constructed only when `gh auth status` succeeds.
pub struct GhCliBacked {
    process: GhCliProcess,
    available: bool,
}

impl GhCliBacked {
    /// Attempt to create a `GhCliBacked`. Returns `None` if gh is not installed
    /// or not authenticated.
    pub fn try_new(work_dir: &Path, binary: Option<&str>) -> Option<Self> {
        let binary = binary.unwrap_or("gh").to_string();
        let process = GhCliProcess {
            binary,
            work_dir: work_dir.to_path_buf(),
        };

        // Check auth status
        match process.run(&["auth", "status"]) {
            Ok(_) => {}
            Err(GitError::GhNotInstalled) => return None,
            Err(_) => {
                return Some(Self {
                    process,
                    available: false,
                });
            }
        }

        // Verify this is a GitHub repo
        match process.run(&["repo", "view", "--json", "owner"]) {
            Ok(_) => Some(Self {
                process,
                available: true,
            }),
            Err(_) => Some(Self {
                process,
                available: false,
            }),
        }
    }

    /// PR list JSON fields used across queries.
    const PR_FIELDS: &str = "number,title,state,headRefName,baseRefName,author,url,isDraft,reviewDecision,additions,deletions,labels,createdAt,updatedAt";
}

impl GitForgeOps for GhCliBacked {
    fn forge_type(&self) -> ForgeType {
        ForgeType::GitHub
    }

    fn is_available(&self) -> bool {
        self.available
    }

    fn list_prs(&self, filters: &PrFilters) -> GitResult<Vec<PullRequest>> {
        let mut args = vec![
            "pr", "list", "--json", Self::PR_FIELDS,
        ];

        let limit_str;
        if let Some(limit) = filters.limit {
            limit_str = limit.to_string();
            args.extend_from_slice(&["--limit", &limit_str]);
        }

        let state_str;
        if let Some(state) = &filters.state {
            state_str = match state {
                crate::git::types::PrState::Open => "open",
                crate::git::types::PrState::Closed => "closed",
                crate::git::types::PrState::Merged => "merged",
            };
            args.extend_from_slice(&["--state", state_str]);
        }

        if let Some(head) = &filters.head {
            args.extend_from_slice(&["--head", head]);
        }
        if let Some(base) = &filters.base {
            args.extend_from_slice(&["--base", base]);
        }

        self.process.run_json(&args)
    }

    fn get_pr(&self, number: u32) -> GitResult<PullRequest> {
        let num_str = number.to_string();
        self.process.run_json(&[
            "pr", "view", &num_str, "--json", Self::PR_FIELDS,
        ])
    }

    fn create_pr(&self, opts: &CreatePrOptions) -> GitResult<PullRequest> {
        let mut args = vec![
            "pr", "create",
            "--title", &opts.title,
            "--body", &opts.body,
            "--base", &opts.base,
            "--head", &opts.head,
        ];
        if opts.draft {
            args.push("--draft");
        }

        // gh pr create returns the PR URL on stdout; we need to fetch it after
        let output = self.process.run(&args)?;
        let url = output.trim();

        // Extract PR number from URL (last path segment)
        let number: u32 = url
            .rsplit('/')
            .next()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| {
                GitError::GhError(format!("could not parse PR number from: {url}"))
            })?;

        self.get_pr(number)
    }

    fn checkout_pr(&self, number: u32) -> GitResult<()> {
        let num_str = number.to_string();
        self.process.run(&["pr", "checkout", &num_str])?;
        Ok(())
    }

    fn merge_pr(&self, number: u32, method: MergeMethod) -> GitResult<()> {
        let num_str = number.to_string();
        let method_flag = match method {
            MergeMethod::Merge => "--merge",
            MergeMethod::Squash => "--squash",
            MergeMethod::Rebase => "--rebase",
        };
        self.process.run(&["pr", "merge", &num_str, method_flag])?;
        Ok(())
    }

    fn pr_for_branch(&self, branch: &str) -> GitResult<Option<PullRequest>> {
        let result: Vec<PullRequest> = self.process.run_json(&[
            "pr", "list",
            "--head", branch,
            "--json", Self::PR_FIELDS,
            "--limit", "1",
        ])?;
        Ok(result.into_iter().next())
    }

    fn pr_checks(&self, number: u32) -> GitResult<Vec<CheckRun>> {
        let num_str = number.to_string();
        self.process.run_json(&[
            "pr", "checks", &num_str,
            "--json", "name,status,conclusion,detailsUrl",
        ])
    }

    fn list_issues(&self, filters: &IssueFilters) -> GitResult<Vec<Issue>> {
        let mut args = vec![
            "issue", "list",
            "--json", "number,title,state,author,labels,assignees,url",
        ];

        let limit_str;
        if let Some(limit) = filters.limit {
            limit_str = limit.to_string();
            args.extend_from_slice(&["--limit", &limit_str]);
        }

        let state_str;
        if let Some(state) = &filters.state {
            state_str = match state {
                crate::git::types::IssueState::Open => "open",
                crate::git::types::IssueState::Closed => "closed",
            };
            args.extend_from_slice(&["--state", state_str]);
        }

        self.process.run_json(&args)
    }

    fn create_issue(&self, opts: &CreateIssueOptions) -> GitResult<Issue> {
        let mut args = vec![
            "issue", "create",
            "--title", &opts.title,
            "--body", &opts.body,
        ];

        let labels_joined;
        if !opts.labels.is_empty() {
            labels_joined = opts.labels.join(",");
            args.extend_from_slice(&["--label", &labels_joined]);
        }

        let assignees_joined;
        if !opts.assignees.is_empty() {
            assignees_joined = opts.assignees.join(",");
            args.extend_from_slice(&["--assignee", &assignees_joined]);
        }

        let output = self.process.run(&args)?;
        let url = output.trim();
        let number: u32 = url
            .rsplit('/')
            .next()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| {
                GitError::GhError(format!("could not parse issue number from: {url}"))
            })?;

        // Fetch the created issue
        self.process.run_json(&[
            "issue", "view", &number.to_string(),
            "--json", "number,title,state,author,labels,assignees,url",
        ])
    }

    fn develop_issue(&self, number: u32) -> GitResult<String> {
        let num_str = number.to_string();
        let output = self.process.run(&["issue", "develop", "-c", &num_str])?;
        Ok(output.trim().to_string())
    }

    fn list_runs(&self, filters: &RunFilters) -> GitResult<Vec<WorkflowRun>> {
        let mut args = vec![
            "run", "list",
            "--json", "databaseId,name,status,conclusion,headBranch,event,url,createdAt",
        ];

        let limit_str;
        if let Some(limit) = filters.limit {
            limit_str = limit.to_string();
            args.extend_from_slice(&["--limit", &limit_str]);
        }

        if let Some(branch) = &filters.branch {
            args.extend_from_slice(&["--branch", branch]);
        }

        self.process.run_json(&args)
    }

    fn rerun_workflow(&self, run_id: u64) -> GitResult<()> {
        let id_str = run_id.to_string();
        self.process.run(&["run", "rerun", &id_str])?;
        Ok(())
    }

    fn run_status(&self, run_id: u64) -> GitResult<WorkflowRun> {
        let id_str = run_id.to_string();
        self.process.run_json(&[
            "run", "view", &id_str,
            "--json", "databaseId,name,status,conclusion,headBranch,event,url,createdAt",
        ])
    }

    fn repo_view(&self) -> GitResult<RepoMetadata> {
        #[derive(serde::Deserialize)]
        struct RawRepoView {
            #[serde(default)]
            owner: OwnerField,
            #[serde(default)]
            name: String,
            #[serde(default, rename = "defaultBranchRef")]
            default_branch_ref: Option<BranchRef>,
            #[serde(default)]
            url: String,
            #[serde(default, rename = "isFork")]
            is_fork: bool,
        }

        #[derive(serde::Deserialize, Default)]
        struct OwnerField {
            #[serde(default)]
            login: String,
        }

        #[derive(serde::Deserialize)]
        struct BranchRef {
            #[serde(default)]
            name: String,
        }

        let raw: RawRepoView = self.process.run_json(&[
            "repo", "view",
            "--json", "owner,name,defaultBranchRef,url,isFork",
        ])?;

        Ok(RepoMetadata {
            owner: raw.owner.login,
            name: raw.name,
            default_branch: raw
                .default_branch_ref
                .map(|r| r.name)
                .unwrap_or_else(|| "main".to_string()),
            url: raw.url,
            is_fork: raw.is_fork,
        })
    }

    fn api_request(
        &self,
        method: &str,
        endpoint: &str,
        body: Option<&str>,
    ) -> GitResult<serde_json::Value> {
        let mut args = vec!["api", "-X", method, endpoint];
        if let Some(b) = body {
            args.extend_from_slice(&["-f", b]);
        }
        self.process.run_json(&args)
    }
}

// ---------------------------------------------------------------------------
// FakeGhCliBacked — test double
// ---------------------------------------------------------------------------

/// In-memory test double for `GitForgeOps`.
pub struct FakeGhCliBacked {
    available: bool,
    prs: Mutex<Vec<PullRequest>>,
    issues: Mutex<Vec<Issue>>,
    runs: Mutex<Vec<WorkflowRun>>,
    checks: Mutex<Vec<CheckRun>>,
}

impl FakeGhCliBacked {
    pub fn new() -> Self {
        Self {
            available: true,
            prs: Mutex::new(Vec::new()),
            issues: Mutex::new(Vec::new()),
            runs: Mutex::new(Vec::new()),
            checks: Mutex::new(Vec::new()),
        }
    }

    pub fn unavailable() -> Self {
        Self {
            available: false,
            prs: Mutex::new(Vec::new()),
            issues: Mutex::new(Vec::new()),
            runs: Mutex::new(Vec::new()),
            checks: Mutex::new(Vec::new()),
        }
    }

    pub fn set_prs(&self, prs: Vec<PullRequest>) {
        *self.prs.lock().unwrap() = prs;
    }

    pub fn set_issues(&self, issues: Vec<Issue>) {
        *self.issues.lock().unwrap() = issues;
    }

    pub fn set_runs(&self, runs: Vec<WorkflowRun>) {
        *self.runs.lock().unwrap() = runs;
    }

    pub fn set_checks(&self, checks: Vec<CheckRun>) {
        *self.checks.lock().unwrap() = checks;
    }
}

impl GitForgeOps for FakeGhCliBacked {
    fn forge_type(&self) -> ForgeType {
        ForgeType::GitHub
    }

    fn is_available(&self) -> bool {
        self.available
    }

    fn list_prs(&self, filters: &PrFilters) -> GitResult<Vec<PullRequest>> {
        let prs = self.prs.lock().unwrap();
        let mut result: Vec<PullRequest> = prs
            .iter()
            .filter(|pr| {
                if let Some(head) = &filters.head {
                    if pr.head_branch != *head {
                        return false;
                    }
                }
                if let Some(state) = &filters.state {
                    if pr.state.as_ref() != Some(state) {
                        return false;
                    }
                }
                true
            })
            .cloned()
            .collect();
        if let Some(limit) = filters.limit {
            result.truncate(limit as usize);
        }
        Ok(result)
    }

    fn get_pr(&self, number: u32) -> GitResult<PullRequest> {
        self.prs
            .lock()
            .unwrap()
            .iter()
            .find(|pr| pr.number == number)
            .cloned()
            .ok_or_else(|| GitError::GhError(format!("PR #{number} not found")))
    }

    fn create_pr(&self, opts: &CreatePrOptions) -> GitResult<PullRequest> {
        let mut prs = self.prs.lock().unwrap();
        let next_number = prs.len() as u32 + 1;
        let pr = PullRequest {
            number: next_number,
            title: opts.title.clone(),
            state: Some(crate::git::types::PrState::Open),
            head_branch: opts.head.clone(),
            base_branch: opts.base.clone(),
            author: None,
            url: format!("https://github.com/test/repo/pull/{next_number}"),
            is_draft: opts.draft,
            review_decision: None,
            additions: 0,
            deletions: 0,
            checks_status: None,
            labels: vec![],
            created_at: String::new(),
            updated_at: String::new(),
        };
        prs.push(pr.clone());
        Ok(pr)
    }

    fn checkout_pr(&self, _number: u32) -> GitResult<()> {
        Ok(())
    }

    fn merge_pr(&self, number: u32, _method: MergeMethod) -> GitResult<()> {
        let mut prs = self.prs.lock().unwrap();
        if let Some(pr) = prs.iter_mut().find(|pr| pr.number == number) {
            pr.state = Some(crate::git::types::PrState::Merged);
        }
        Ok(())
    }

    fn pr_for_branch(&self, branch: &str) -> GitResult<Option<PullRequest>> {
        Ok(self
            .prs
            .lock()
            .unwrap()
            .iter()
            .find(|pr| pr.head_branch == branch)
            .cloned())
    }

    fn pr_checks(&self, _number: u32) -> GitResult<Vec<CheckRun>> {
        Ok(self.checks.lock().unwrap().clone())
    }

    fn list_issues(&self, filters: &IssueFilters) -> GitResult<Vec<Issue>> {
        let issues = self.issues.lock().unwrap();
        let mut result: Vec<Issue> = issues.clone();
        if let Some(limit) = filters.limit {
            result.truncate(limit as usize);
        }
        Ok(result)
    }

    fn create_issue(&self, opts: &CreateIssueOptions) -> GitResult<Issue> {
        let issue = Issue {
            number: self.issues.lock().unwrap().len() as u32 + 1,
            title: opts.title.clone(),
            state: Some(crate::git::types::IssueState::Open),
            author: None,
            labels: opts
                .labels
                .iter()
                .map(|l| crate::git::types::LabelItem { name: l.clone() })
                .collect(),
            assignees: opts
                .assignees
                .iter()
                .map(|a| crate::git::types::PrAuthor { login: a.clone() })
                .collect(),
            url: String::new(),
        };
        self.issues.lock().unwrap().push(issue.clone());
        Ok(issue)
    }

    fn develop_issue(&self, number: u32) -> GitResult<String> {
        Ok(format!("{number}-issue-branch"))
    }

    fn list_runs(&self, filters: &RunFilters) -> GitResult<Vec<WorkflowRun>> {
        let runs = self.runs.lock().unwrap();
        let mut result: Vec<WorkflowRun> = runs
            .iter()
            .filter(|r| {
                if let Some(branch) = &filters.branch {
                    r.head_branch == *branch
                } else {
                    true
                }
            })
            .cloned()
            .collect();
        if let Some(limit) = filters.limit {
            result.truncate(limit as usize);
        }
        Ok(result)
    }

    fn rerun_workflow(&self, _run_id: u64) -> GitResult<()> {
        Ok(())
    }

    fn run_status(&self, run_id: u64) -> GitResult<WorkflowRun> {
        self.runs
            .lock()
            .unwrap()
            .iter()
            .find(|r| r.id == run_id)
            .cloned()
            .ok_or_else(|| GitError::GhError(format!("run {run_id} not found")))
    }

    fn repo_view(&self) -> GitResult<RepoMetadata> {
        Ok(RepoMetadata {
            owner: "test-owner".to_string(),
            name: "test-repo".to_string(),
            default_branch: "main".to_string(),
            url: "https://github.com/test-owner/test-repo".to_string(),
            is_fork: false,
        })
    }

    fn api_request(
        &self,
        _method: &str,
        _endpoint: &str,
        _body: Option<&str>,
    ) -> GitResult<serde_json::Value> {
        Ok(serde_json::Value::Null)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::types::PrState;

    #[test]
    fn test_fake_create_and_list_prs() {
        let fake = FakeGhCliBacked::new();
        assert!(fake.is_available());

        let pr = fake
            .create_pr(&CreatePrOptions {
                title: "Test PR".into(),
                body: "Body".into(),
                base: "main".into(),
                head: "feature".into(),
                draft: false,
            })
            .unwrap();
        assert_eq!(pr.number, 1);
        assert_eq!(pr.title, "Test PR");

        let prs = fake.list_prs(&PrFilters::default()).unwrap();
        assert_eq!(prs.len(), 1);
    }

    #[test]
    fn test_fake_pr_for_branch() {
        let fake = FakeGhCliBacked::new();
        fake.set_prs(vec![PullRequest {
            number: 42,
            title: "My PR".into(),
            state: Some(PrState::Open),
            head_branch: "feature-x".into(),
            base_branch: "main".into(),
            author: None,
            url: String::new(),
            is_draft: false,
            review_decision: None,
            additions: 10,
            deletions: 5,
            checks_status: None,
            labels: vec![],
            created_at: String::new(),
            updated_at: String::new(),
        }]);

        let pr = fake.pr_for_branch("feature-x").unwrap();
        assert_eq!(pr.unwrap().number, 42);

        let pr = fake.pr_for_branch("other-branch").unwrap();
        assert!(pr.is_none());
    }

    #[test]
    fn test_fake_merge_pr() {
        let fake = FakeGhCliBacked::new();
        fake.create_pr(&CreatePrOptions {
            title: "Merge me".into(),
            body: "".into(),
            base: "main".into(),
            head: "fix".into(),
            draft: false,
        })
        .unwrap();

        fake.merge_pr(1, MergeMethod::Squash).unwrap();
        let pr = fake.get_pr(1).unwrap();
        assert_eq!(pr.state, Some(PrState::Merged));
    }

    #[test]
    fn test_fake_unavailable() {
        let fake = FakeGhCliBacked::unavailable();
        assert!(!fake.is_available());
    }
}
