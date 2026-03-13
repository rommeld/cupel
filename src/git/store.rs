use std::collections::HashMap;
use std::sync::Arc;

use gpui::{AppContext, Context, Entity, EventEmitter, Subscription};

use crate::git::repository::GitRepository;
use crate::git::repo_entity::{Repository, RepositoryEvent};

// ---------------------------------------------------------------------------
// RepositoryId
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RepositoryId(u64);

// ---------------------------------------------------------------------------
// GitStoreEvent
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub enum GitStoreEvent {
    ActiveRepositoryChanged,
    StatusesChanged(RepositoryId),
    CommitCompleted {
        repository_id: RepositoryId,
        was_amend: bool,
    },
    BranchChanged,
}

// ---------------------------------------------------------------------------
// GitStore
// ---------------------------------------------------------------------------

pub struct GitStore {
    repositories: HashMap<RepositoryId, Entity<Repository>>,
    active_repository: Option<RepositoryId>,
    next_repository_id: u64,
    subscriptions: HashMap<RepositoryId, Subscription>,
}

impl EventEmitter<GitStoreEvent> for GitStore {}

impl GitStore {
    pub fn new(_cx: &mut Context<Self>) -> Self {
        Self {
            repositories: HashMap::new(),
            active_repository: None,
            next_repository_id: 0,
            subscriptions: HashMap::new(),
        }
    }

    /// Add a repository to the store. Returns its ID.
    /// Sets it as active if it's the first repository.
    pub fn add_repository(
        &mut self,
        git_repo: Arc<dyn GitRepository>,
        cx: &mut Context<Self>,
    ) -> RepositoryId {
        let id = RepositoryId(self.next_repository_id);
        self.next_repository_id += 1;

        let repo_entity = cx.new(|cx| Repository::new(git_repo, cx));

        // Subscribe to RepositoryEvent for all state changes.
        // No separate observe() needed — the subscribe handler covers all events.
        let event_sub = cx.subscribe(&repo_entity, {
            move |_this: &mut Self, _repo: Entity<Repository>, event: &RepositoryEvent, cx| {
                match event {
                    RepositoryEvent::StatusChanged => {
                        cx.emit(GitStoreEvent::StatusesChanged(id));
                    }
                    RepositoryEvent::IndexChanged => {
                        cx.emit(GitStoreEvent::StatusesChanged(id));
                    }
                    RepositoryEvent::CommitCompleted { was_amend } => {
                        cx.emit(GitStoreEvent::CommitCompleted {
                            repository_id: id,
                            was_amend: *was_amend,
                        });
                    }
                    RepositoryEvent::BranchChanged => {
                        cx.emit(GitStoreEvent::BranchChanged);
                    }
                }
            }
        });

        self.subscriptions.insert(id, event_sub);
        self.repositories.insert(id, repo_entity);

        // First repository becomes active
        if self.active_repository.is_none() {
            self.active_repository = Some(id);
            cx.emit(GitStoreEvent::ActiveRepositoryChanged);
        }

        id
    }

    /// Remove a repository from the store.
    pub fn remove_repository(
        &mut self,
        id: RepositoryId,
        cx: &mut Context<Self>,
    ) {
        self.repositories.remove(&id);
        self.subscriptions.remove(&id);

        if self.active_repository == Some(id) {
            // Pick another repository or None
            self.active_repository = self.repositories.keys().next().copied();
            cx.emit(GitStoreEvent::ActiveRepositoryChanged);
        }
    }

    /// Get the active repository entity.
    pub fn active_repository(&self) -> Option<&Entity<Repository>> {
        self.active_repository
            .and_then(|id| self.repositories.get(&id))
    }

    /// Get the active repository ID.
    pub fn active_repository_id(&self) -> Option<RepositoryId> {
        self.active_repository
    }

    /// Set the active repository by ID.
    pub fn set_active_repository(
        &mut self,
        id: RepositoryId,
        cx: &mut Context<Self>,
    ) {
        if self.repositories.contains_key(&id) && self.active_repository != Some(id) {
            self.active_repository = Some(id);
            cx.emit(GitStoreEvent::ActiveRepositoryChanged);
        }
    }

    /// Get a repository by ID.
    pub fn repository(&self, id: RepositoryId) -> Option<&Entity<Repository>> {
        self.repositories.get(&id)
    }

    /// Iterate all repositories.
    pub fn repositories(
        &self,
    ) -> impl Iterator<Item = (RepositoryId, &Entity<Repository>)> {
        self.repositories.iter().map(|(id, repo)| (*id, repo))
    }

    /// Number of tracked repositories.
    pub fn repository_count(&self) -> usize {
        self.repositories.len()
    }
}
