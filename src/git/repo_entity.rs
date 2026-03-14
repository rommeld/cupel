use std::collections::HashMap;
use std::sync::Arc;

use gpui::{Context, EventEmitter, Task};
use tokio::sync::mpsc;

use crate::git::backend_router::{BackendRouter, GitForgeOps};
use crate::git::forge_state::{ForgeState, ForgeStateSnapshot};
use crate::git::types::{
    Branch, CommitOptions, CreatePrOptions, MergeMethod, PrFilters, PushOptions, RepoPath,
    RunFilters, StagingState, StatusEntry,
};

// ---------------------------------------------------------------------------
// RepositoryEvent
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub enum RepositoryEvent {
    StatusChanged,
    IndexChanged,
    CommitCompleted { was_amend: bool },
    BranchChanged,
    ForgeStateUpdated,
}

// ---------------------------------------------------------------------------
// PendingOp — optimistic UI state
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PendingOp {
    Staging,
    Unstaging,
}

// ---------------------------------------------------------------------------
// RepositorySnapshot — cheap clone for UI reads
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default)]
pub struct RepositorySnapshot {
    pub branch: Option<Branch>,
    pub stash_count: usize,
}

// ---------------------------------------------------------------------------
// GitJob — job queue entries
// ---------------------------------------------------------------------------

enum GitJob {
    RefreshStatus,
    Stage {
        paths: Vec<RepoPath>,
    },
    Unstage {
        paths: Vec<RepoPath>,
    },
    SetIndexText {
        path: RepoPath,
        content: Option<String>,
    },
    Commit {
        message: String,
        options: CommitOptions,
    },
    Push {
        branch: String,
        remote: Option<String>,
        options: PushOptions,
    },
    Pull {
        rebase: bool,
    },
    Fetch,
    // ── Forge jobs ─────────────────────────────────────────
    RefreshForgeState,
    CreatePr {
        opts: CreatePrOptions,
    },
    CheckoutPr {
        number: u32,
    },
    MergePr {
        number: u32,
        method: MergeMethod,
    },
    DevelopIssue {
        number: u32,
    },
    RerunWorkflow {
        run_id: u64,
    },
}

// ---------------------------------------------------------------------------
// Repository entity
// ---------------------------------------------------------------------------

pub struct Repository {
    pub(crate) statuses: Vec<StatusEntry>,
    pub(crate) snapshot: RepositorySnapshot,
    pub(crate) pending_ops: HashMap<RepoPath, PendingOp>,
    pub(crate) forge_snapshot: ForgeStateSnapshot,

    forge_state: Option<ForgeState>,
    router: BackendRouter,
    job_sender: mpsc::UnboundedSender<GitJob>,
    _worker: Task<()>,
    _poll_task: Option<Task<()>>,
}

impl EventEmitter<RepositoryEvent> for Repository {}

impl Repository {
    pub fn new(
        router: BackendRouter,
        cx: &mut Context<Self>,
    ) -> Self {
        let (job_sender, mut job_receiver) = mpsc::unbounded_channel::<GitJob>();
        let backend = router.clone();

        let worker = cx.spawn(async move |entity, cx| {
            let env = HashMap::new();
            while let Some(job) = job_receiver.recv().await {
                let local = backend.local();
                let remote = backend.remote();

                match job {
                    GitJob::RefreshStatus => {
                        let entries = local.status(&[]).unwrap_or_default();
                        let branch = local.current_branch();
                        entity
                            .update(cx, |repo: &mut Repository, cx| {
                                repo.statuses = entries;
                                repo.snapshot.branch = branch;
                                cx.emit(RepositoryEvent::StatusChanged);
                                cx.notify();
                            })
                            .ok();
                    }
                    GitJob::Stage { paths } => {
                        if let Err(e) = local.stage_paths(&paths, &env) {
                            eprintln!("git stage failed: {e}");
                        }
                        let entries = local.status(&[]).unwrap_or_default();
                        entity
                            .update(cx, |repo: &mut Repository, cx| {
                                repo.statuses = entries;
                                for p in &paths {
                                    repo.pending_ops.remove(p);
                                }
                                cx.emit(RepositoryEvent::IndexChanged);
                                cx.notify();
                            })
                            .ok();
                    }
                    GitJob::Unstage { paths } => {
                        if let Err(e) = local.unstage_paths(&paths, &env) {
                            eprintln!("git unstage failed: {e}");
                        }
                        let entries = local.status(&[]).unwrap_or_default();
                        entity
                            .update(cx, |repo: &mut Repository, cx| {
                                repo.statuses = entries;
                                for p in &paths {
                                    repo.pending_ops.remove(p);
                                }
                                cx.emit(RepositoryEvent::IndexChanged);
                                cx.notify();
                            })
                            .ok();
                    }
                    GitJob::SetIndexText { path, content } => {
                        if let Err(e) = local.set_index_text(&path, content, &env) {
                            eprintln!("git set_index_text failed: {e}");
                        }
                        let entries = local.status(&[]).unwrap_or_default();
                        entity
                            .update(cx, |repo: &mut Repository, cx| {
                                repo.statuses = entries;
                                cx.emit(RepositoryEvent::IndexChanged);
                                cx.notify();
                            })
                            .ok();
                    }
                    GitJob::Commit { message, options } => {
                        let was_amend = options.amend;
                        if let Err(e) = local.commit(&message, &options, &env) {
                            eprintln!("git commit failed: {e}");
                        }
                        let entries = local.status(&[]).unwrap_or_default();
                        let branch = local.current_branch();
                        entity
                            .update(cx, |repo: &mut Repository, cx| {
                                repo.statuses = entries;
                                repo.snapshot.branch = branch;
                                cx.emit(RepositoryEvent::CommitCompleted { was_amend });
                                cx.notify();
                            })
                            .ok();
                    }
                    GitJob::Push {
                        branch,
                        remote: remote_name,
                        options,
                    } => {
                        if let Err(e) = remote.push(
                            &branch,
                            remote_name.as_deref(),
                            &options,
                            &env,
                        ) {
                            eprintln!("git push failed: {e}");
                        }
                    }
                    GitJob::Pull { rebase } => {
                        if let Err(e) = remote.pull(rebase, &env) {
                            eprintln!("git pull failed: {e}");
                        }
                        let entries = local.status(&[]).unwrap_or_default();
                        let branch = local.current_branch();
                        entity
                            .update(cx, |repo: &mut Repository, cx| {
                                repo.statuses = entries;
                                repo.snapshot.branch = branch;
                                cx.emit(RepositoryEvent::BranchChanged);
                                cx.notify();
                            })
                            .ok();
                    }
                    GitJob::Fetch => {
                        if let Err(e) = remote.fetch(&env) {
                            eprintln!("git fetch failed: {e}");
                        }
                    }

                    // ── Forge jobs ──────────────────────────────────
                    GitJob::RefreshForgeState => {
                        if let Some(forge) = backend.forge() {
                            Self::do_forge_refresh(forge, &entity, cx).await;
                        }
                    }
                    GitJob::CreatePr { opts } => {
                        if let Some(forge) = backend.forge() {
                            match forge.create_pr(&opts) {
                                Ok(pr) => {
                                    entity
                                        .update(cx, |repo: &mut Repository, cx| {
                                            repo.forge_snapshot.current_pr = Some(pr);
                                            cx.emit(RepositoryEvent::ForgeStateUpdated);
                                            cx.notify();
                                        })
                                        .ok();
                                    Self::do_forge_refresh(forge, &entity, cx).await;
                                }
                                Err(e) => {
                                    eprintln!("PR creation failed: {e}");
                                    entity
                                        .update(cx, |repo: &mut Repository, cx| {
                                            repo.forge_snapshot.last_error =
                                                Some(format!("PR creation failed: {e}"));
                                            cx.emit(RepositoryEvent::ForgeStateUpdated);
                                            cx.notify();
                                        })
                                        .ok();
                                }
                            }
                        }
                    }
                    GitJob::CheckoutPr { number } => {
                        if let Some(forge) = backend.forge() {
                            match forge.checkout_pr(number) {
                                Ok(()) => {
                                    let entries = local.status(&[]).unwrap_or_default();
                                    let branch = local.current_branch();
                                    entity
                                        .update(cx, |repo: &mut Repository, cx| {
                                            repo.statuses = entries;
                                            repo.snapshot.branch = branch;
                                            cx.emit(RepositoryEvent::BranchChanged);
                                            cx.notify();
                                        })
                                        .ok();
                                    Self::do_forge_refresh(forge, &entity, cx).await;
                                }
                                Err(e) => eprintln!("PR checkout failed: {e}"),
                            }
                        }
                    }
                    GitJob::MergePr { number, method } => {
                        if let Some(forge) = backend.forge() {
                            match forge.merge_pr(number, method) {
                                Ok(()) => {
                                    Self::do_forge_refresh(forge, &entity, cx).await;
                                }
                                Err(e) => {
                                    eprintln!("PR merge failed: {e}");
                                    entity
                                        .update(cx, |repo: &mut Repository, cx| {
                                            repo.forge_snapshot.last_error =
                                                Some(format!("PR merge failed: {e}"));
                                            cx.emit(RepositoryEvent::ForgeStateUpdated);
                                            cx.notify();
                                        })
                                        .ok();
                                }
                            }
                        }
                    }
                    GitJob::DevelopIssue { number } => {
                        if let Some(forge) = backend.forge() {
                            match forge.develop_issue(number) {
                                Ok(branch_name) => {
                                    eprintln!("Created branch: {branch_name}");
                                    let entries = local.status(&[]).unwrap_or_default();
                                    let branch = local.current_branch();
                                    entity
                                        .update(cx, |repo: &mut Repository, cx| {
                                            repo.statuses = entries;
                                            repo.snapshot.branch = branch;
                                            cx.emit(RepositoryEvent::BranchChanged);
                                            cx.notify();
                                        })
                                        .ok();
                                }
                                Err(e) => eprintln!("Issue develop failed: {e}"),
                            }
                        }
                    }
                    GitJob::RerunWorkflow { run_id } => {
                        if let Some(forge) = backend.forge() {
                            if let Err(e) = forge.rerun_workflow(run_id) {
                                eprintln!("Workflow rerun failed: {e}");
                            }
                            Self::do_forge_refresh(forge, &entity, cx).await;
                        }
                    }
                }
            }
        });

        // Queue initial status refresh
        let _ = job_sender.send(GitJob::RefreshStatus);

        // Queue initial forge state refresh if forge is available
        if router.has_forge() {
            let _ = job_sender.send(GitJob::RefreshForgeState);
        }

        // Spawn a background polling task for periodic forge state refresh
        let (forge_state, poll_task) = if router.has_forge() {
            let sender = job_sender.clone();
            let state = ForgeState::new(60);
            let poll_interval = state.poll_interval;
            let task = cx.spawn(async move |entity, cx| {
                loop {
                    // Read the current poll interval from ForgeState
                    let interval = entity
                        .update(cx, |repo: &mut Repository, _cx| {
                            repo.forge_state
                                .as_ref()
                                .map(|s| s.poll_interval)
                                .unwrap_or(poll_interval)
                        })
                        .unwrap_or(poll_interval);
                    smol::Timer::after(interval).await;
                    if sender.send(GitJob::RefreshForgeState).is_err() {
                        break;
                    }
                }
            });
            (Some(state), Some(task))
        } else {
            (None, None)
        };

        Self {
            statuses: Vec::new(),
            snapshot: RepositorySnapshot::default(),
            pending_ops: HashMap::new(),
            forge_snapshot: ForgeStateSnapshot::default(),
            forge_state,
            router,
            job_sender,
            _worker: worker,
            _poll_task: poll_task,
        }
    }

    /// Perform a forge state refresh. Called from the worker task.
    async fn do_forge_refresh(
        forge: &dyn GitForgeOps,
        entity: &gpui::WeakEntity<Repository>,
        cx: &mut gpui::AsyncApp,
    ) {
        // Get current branch name for filtering
        let branch_name = entity
            .update(cx, |repo: &mut Repository, _cx| {
                repo.snapshot
                    .branch
                    .as_ref()
                    .map(|b| b.name.clone())
            })
            .ok()
            .flatten();

        // Fetch PR for current branch
        let current_pr = branch_name
            .as_ref()
            .and_then(|branch| forge.pr_for_branch(branch).ok().flatten());

        // Fetch checks for current PR
        let pr_checks = current_pr
            .as_ref()
            .map(|pr| forge.pr_checks(pr.number).unwrap_or_default())
            .unwrap_or_default();

        // Fetch open PRs
        let open_prs = forge
            .list_prs(&PrFilters {
                state: Some(crate::git::types::PrState::Open),
                limit: Some(20),
                ..Default::default()
            })
            .unwrap_or_default();

        // Fetch recent workflow runs for current branch
        let recent_runs = forge
            .list_runs(&RunFilters {
                branch: branch_name,
                limit: Some(10),
            })
            .unwrap_or_default();

        entity
            .update(cx, |repo: &mut Repository, cx| {
                repo.forge_snapshot = ForgeStateSnapshot {
                    current_pr,
                    open_prs: Arc::new(open_prs),
                    pr_checks: Arc::new(pr_checks),
                    recent_runs: Arc::new(recent_runs),
                    is_loading: false,
                    last_error: None,
                };
                if let Some(state) = &mut repo.forge_state {
                    state.record_success();
                }
                cx.emit(RepositoryEvent::ForgeStateUpdated);
                cx.notify();
            })
            .ok();
    }

    /// Stage files with optimistic UI update.
    pub fn stage(&mut self, paths: Vec<RepoPath>, cx: &mut Context<Self>) {
        for path in &paths {
            self.pending_ops.insert(path.clone(), PendingOp::Staging);
        }
        cx.notify();
        let _ = self.job_sender.send(GitJob::Stage { paths });
    }

    /// Unstage files with optimistic UI update.
    pub fn unstage(&mut self, paths: Vec<RepoPath>, cx: &mut Context<Self>) {
        for path in &paths {
            self.pending_ops.insert(path.clone(), PendingOp::Unstaging);
        }
        cx.notify();
        let _ = self.job_sender.send(GitJob::Unstage { paths });
    }

    /// Write specific content to the index for a path.
    pub fn set_index_text(
        &mut self,
        path: RepoPath,
        content: Option<String>,
    ) {
        let _ = self
            .job_sender
            .send(GitJob::SetIndexText { path, content });
    }

    /// Commit staged changes.
    pub fn commit(
        &mut self,
        message: String,
        options: CommitOptions,
        _cx: &mut Context<Self>,
    ) {
        let _ = self
            .job_sender
            .send(GitJob::Commit { message, options });
    }

    /// Push to remote.
    pub fn push(
        &self,
        branch: String,
        remote: Option<String>,
        options: PushOptions,
    ) {
        let _ = self.job_sender.send(GitJob::Push {
            branch,
            remote,
            options,
        });
    }

    /// Pull from remote.
    pub fn pull(&self, rebase: bool) {
        let _ = self.job_sender.send(GitJob::Pull { rebase });
    }

    /// Fetch from remote.
    pub fn fetch(&self) {
        let _ = self.job_sender.send(GitJob::Fetch);
    }

    /// Trigger a status refresh.
    pub fn refresh_status(&self) {
        let _ = self.job_sender.send(GitJob::RefreshStatus);
    }

    /// Trigger a forge state refresh.
    pub fn refresh_forge_state(&self) {
        let _ = self.job_sender.send(GitJob::RefreshForgeState);
    }

    /// Create a pull request.
    pub fn create_pr(&self, opts: CreatePrOptions) {
        let _ = self.job_sender.send(GitJob::CreatePr { opts });
    }

    /// Checkout a pull request by number.
    pub fn checkout_pr(&self, number: u32) {
        let _ = self.job_sender.send(GitJob::CheckoutPr { number });
    }

    /// Merge a pull request.
    pub fn merge_pr(&self, number: u32, method: MergeMethod) {
        let _ = self.job_sender.send(GitJob::MergePr { number, method });
    }

    /// Create a linked branch from an issue.
    pub fn develop_issue(&self, number: u32) {
        let _ = self.job_sender.send(GitJob::DevelopIssue { number });
    }

    /// Re-run a failed workflow.
    pub fn rerun_workflow(&self, run_id: u64) {
        let _ = self.job_sender.send(GitJob::RerunWorkflow { run_id });
    }

    /// Get the current snapshot for cheap UI reads.
    pub fn snapshot(&self) -> &RepositorySnapshot {
        &self.snapshot
    }

    /// Get current status entries.
    pub fn statuses(&self) -> &[StatusEntry] {
        &self.statuses
    }

    /// Get the effective staging state for a path, accounting for pending ops.
    pub fn effective_staging_state(&self, path: &RepoPath) -> StagingState {
        if let Some(pending) = self.pending_ops.get(path) {
            return match pending {
                PendingOp::Staging => StagingState::Staged,
                PendingOp::Unstaging => StagingState::Unstaged,
            };
        }

        self.statuses
            .binary_search_by(|s| s.repo_path.cmp(path))
            .ok()
            .map(|idx| self.statuses[idx].staging_state())
            .unwrap_or(StagingState::Unstaged)
    }

    /// Get the backend router.
    pub fn router(&self) -> &BackendRouter {
        &self.router
    }

    /// Get the forge state snapshot.
    pub fn forge_snapshot(&self) -> &ForgeStateSnapshot {
        &self.forge_snapshot
    }
}
