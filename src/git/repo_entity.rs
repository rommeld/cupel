use std::collections::HashMap;
use std::sync::Arc;

use gpui::{Context, EventEmitter, Task};
use tokio::sync::mpsc;

use crate::git::repository::GitRepository;
use crate::git::types::{
    Branch, CommitOptions, PushOptions, RepoPath, StagingState, StatusEntry,
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
}

// ---------------------------------------------------------------------------
// Repository entity
// ---------------------------------------------------------------------------

pub struct Repository {
    pub statuses: Vec<StatusEntry>,
    pub snapshot: RepositorySnapshot,
    pub pending_ops: HashMap<RepoPath, PendingOp>,

    git_repo: Arc<dyn GitRepository>,
    job_sender: mpsc::UnboundedSender<GitJob>,
    _worker: Task<()>,
}

impl EventEmitter<RepositoryEvent> for Repository {}

impl Repository {
    pub fn new(
        git_repo: Arc<dyn GitRepository>,
        cx: &mut Context<Self>,
    ) -> Self {
        let (job_sender, mut job_receiver) = mpsc::unbounded_channel::<GitJob>();
        let backend = git_repo.clone();

        let worker = cx.spawn(async move |entity, cx| {
            let env = HashMap::new();
            while let Some(job) = job_receiver.recv().await {
                let backend = &backend;

                match job {
                    GitJob::RefreshStatus => {
                        let entries = backend.status(&[]).unwrap_or_default();
                        let branch = backend.current_branch();
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
                        if let Err(e) = backend.stage_paths(&paths, &env) {
                            eprintln!("git stage failed: {e}");
                        }
                        let entries = backend.status(&[]).unwrap_or_default();
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
                        if let Err(e) = backend.unstage_paths(&paths, &env) {
                            eprintln!("git unstage failed: {e}");
                        }
                        let entries = backend.status(&[]).unwrap_or_default();
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
                        if let Err(e) = backend.set_index_text(&path, content, &env) {
                            eprintln!("git set_index_text failed: {e}");
                        }
                        let entries = backend.status(&[]).unwrap_or_default();
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
                        if let Err(e) = backend.commit(&message, &options, &env) {
                            eprintln!("git commit failed: {e}");
                        }
                        let entries = backend.status(&[]).unwrap_or_default();
                        let branch = backend.current_branch();
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
                        remote,
                        options,
                    } => {
                        if let Err(e) = backend.push(
                            &branch,
                            remote.as_deref(),
                            &options,
                            &env,
                        ) {
                            eprintln!("git push failed: {e}");
                        }
                    }
                    GitJob::Pull { rebase } => {
                        if let Err(e) = backend.pull(rebase, &env) {
                            eprintln!("git pull failed: {e}");
                        }
                        let entries = backend.status(&[]).unwrap_or_default();
                        let branch = backend.current_branch();
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
                        if let Err(e) = backend.fetch(&env) {
                            eprintln!("git fetch failed: {e}");
                        }
                    }
                }
            }
        });

        // Queue initial status refresh
        let _ = job_sender.send(GitJob::RefreshStatus);

        Self {
            statuses: Vec::new(),
            snapshot: RepositorySnapshot::default(),
            pending_ops: HashMap::new(),
            git_repo,
            job_sender,
            _worker: worker,
        }
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

    /// Get the underlying git repository.
    pub fn git_repo(&self) -> &Arc<dyn GitRepository> {
        &self.git_repo
    }
}
