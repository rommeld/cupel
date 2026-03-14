use std::collections::HashMap;

use gpui::{App, AppContext, Context, Entity, EventEmitter, Subscription, WeakEntity};

use crate::git::backend_router::BackendRouter;
use crate::git::diff::{BufferDiff, BufferDiffSnapshot};
use crate::git::forge_state::ForgeStateSnapshot;
use crate::git::repo_entity::{Repository, RepositoryEvent};
use crate::git::types::RepoPath;

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
    ForgeStateChanged,
}

// ---------------------------------------------------------------------------
// RevertInfo — data returned by revert_hunk_text
// ---------------------------------------------------------------------------

/// Information needed to apply a hunk revert to a buffer.
#[derive(Clone, Debug)]
pub struct RevertInfo {
    /// Line range in the buffer to replace.
    pub buffer_line_range: std::ops::Range<u32>,
    /// Text to replace the buffer range with (from base/HEAD).
    pub replacement_text: String,
}

// ---------------------------------------------------------------------------
// BufferId — identifies a buffer for per-buffer diff tracking
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct BufferId(u64);

impl BufferId {
    pub fn new(id: u64) -> Self {
        Self(id)
    }
}

// ---------------------------------------------------------------------------
// BufferGitState — per-buffer diff state
// ---------------------------------------------------------------------------

/// Per-buffer state that tracks base texts and diff entities.
///
/// - `unstaged_diff`: working dir vs index (base = index_text)
/// - `uncommitted_diff`: working dir vs HEAD (base = head_text)
///
/// The operation count mechanism prevents stale worktree watcher reads from
/// overwriting in-flight staging writes to the index.
pub struct BufferGitState {
    /// Diff entity: working dir vs index.
    pub unstaged_diff: WeakEntity<BufferDiff>,
    /// Diff entity: working dir vs HEAD.
    pub uncommitted_diff: WeakEntity<BufferDiff>,

    /// Cached HEAD content for this buffer's file.
    pub head_text: Option<String>,
    /// Cached index content for this buffer's file.
    pub index_text: Option<String>,

    /// Counter incremented on each hunk staging operation.
    pub hunk_staging_operation_count: u64,
    /// Counter value at the time of the last completed write.
    pub hunk_staging_operation_count_as_of_write: u64,

    /// Path in the repo for this buffer.
    pub repo_path: RepoPath,
}

impl BufferGitState {
    /// Increment the operation count to begin a new staging operation.
    pub fn begin_hunk_staging_operation(&mut self) {
        self.hunk_staging_operation_count += 1;
    }

    /// Check if it's safe to accept a disk read (no in-flight operations).
    pub fn can_accept_disk_read(&self) -> bool {
        self.hunk_staging_operation_count == self.hunk_staging_operation_count_as_of_write
    }

    /// Update index text from disk, but only if no staging op is in-flight.
    pub fn maybe_refresh_index_text(&mut self, text_from_disk: Option<String>) -> bool {
        if self.can_accept_disk_read() {
            self.index_text = text_from_disk;
            true
        } else {
            false
        }
    }

    /// Called when a staging write completes. Updates the operation count
    /// and caches the new index text.
    pub fn staging_write_completed(&mut self, new_index_text: Option<String>) {
        self.hunk_staging_operation_count_as_of_write = self.hunk_staging_operation_count;
        self.index_text = new_index_text;
    }
}

// ---------------------------------------------------------------------------
// GitStore
// ---------------------------------------------------------------------------

pub struct GitStore {
    repositories: HashMap<RepositoryId, Entity<Repository>>,
    active_repository: Option<RepositoryId>,
    next_repository_id: u64,
    subscriptions: HashMap<RepositoryId, Subscription>,
    buffer_states: HashMap<BufferId, BufferGitState>,
    next_buffer_id: u64,
}

impl EventEmitter<GitStoreEvent> for GitStore {}

impl GitStore {
    pub fn new(_cx: &mut Context<Self>) -> Self {
        Self {
            repositories: HashMap::new(),
            active_repository: None,
            next_repository_id: 0,
            subscriptions: HashMap::new(),
            buffer_states: HashMap::new(),
            next_buffer_id: 0,
        }
    }

    /// Add a repository to the store. Returns its ID.
    /// Sets it as active if it's the first repository.
    pub fn add_repository(
        &mut self,
        router: BackendRouter,
        cx: &mut Context<Self>,
    ) -> RepositoryId {
        let id = RepositoryId(self.next_repository_id);
        self.next_repository_id += 1;

        let repo_entity = cx.new(|cx| Repository::new(router, cx));

        // Subscribe to RepositoryEvent for all state changes.
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
                    RepositoryEvent::ForgeStateUpdated => {
                        cx.emit(GitStoreEvent::ForgeStateChanged);
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

    /// Get the forge state snapshot for the active repository.
    pub fn forge_snapshot(&self, cx: &App) -> Option<ForgeStateSnapshot> {
        self.active_repository()
            .map(|repo| repo.read(cx).forge_snapshot().clone())
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

    // -- Per-buffer diff state ------------------------------------------------

    /// Register a buffer for diff tracking. Creates unstaged and uncommitted
    /// BufferDiff entities, reads head/index text from the repository, and
    /// returns the BufferId.
    pub fn register_buffer(
        &mut self,
        repo_path: RepoPath,
        current_text: String,
        cx: &mut Context<Self>,
    ) -> BufferId {
        let id = BufferId(self.next_buffer_id);
        self.next_buffer_id += 1;

        // Read head and index text from the active repository.
        let (head_text, index_text) = self.read_base_texts_from_repo(&repo_path, cx);

        // Create the two BufferDiff entities.
        let unstaged_diff = cx.new(|cx| {
            let mut diff = BufferDiff::new(cx);
            diff.set_base_text(index_text.clone(), cx);
            diff.set_current_text(current_text.clone(), cx);
            diff
        });

        let uncommitted_diff = cx.new(|cx| {
            let mut diff = BufferDiff::new(cx);
            diff.set_base_text(head_text.clone(), cx);
            diff.set_current_text(current_text, cx);
            diff
        });

        let state = BufferGitState {
            unstaged_diff: unstaged_diff.downgrade(),
            uncommitted_diff: uncommitted_diff.downgrade(),
            head_text,
            index_text,
            hunk_staging_operation_count: 0,
            hunk_staging_operation_count_as_of_write: 0,
            repo_path,
        };

        self.buffer_states.insert(id, state);
        id
    }

    /// Update the current (working directory) text for a tracked buffer.
    pub fn update_buffer_text(
        &mut self,
        buffer_id: BufferId,
        text: String,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self.buffer_states.get(&buffer_id) else {
            return;
        };

        if let Some(diff) = state.unstaged_diff.upgrade() {
            diff.update(cx, |d, cx| d.set_current_text(text.clone(), cx));
        }
        if let Some(diff) = state.uncommitted_diff.upgrade() {
            diff.update(cx, |d, cx| d.set_current_text(text, cx));
        }
    }

    /// Refresh base texts from the repository (e.g., after worktree change).
    pub fn refresh_buffer_base_texts(
        &mut self,
        buffer_id: BufferId,
        cx: &mut Context<Self>,
    ) {
        let Some(repo_path) = self
            .buffer_states
            .get(&buffer_id)
            .map(|s| s.repo_path.clone())
        else {
            return;
        };

        let (head_text, index_text) = self.read_base_texts_from_repo(&repo_path, cx);

        let Some(state) = self.buffer_states.get_mut(&buffer_id) else {
            return;
        };

        // Always update head text.
        state.head_text = head_text.clone();
        if let Some(diff) = state.uncommitted_diff.upgrade() {
            diff.update(cx, |d, cx| d.set_base_text(head_text, cx));
        }

        // Only update index text if no staging operation is in-flight.
        if state.maybe_refresh_index_text(index_text.clone()) {
            if let Some(diff) = state.unstaged_diff.upgrade() {
                diff.update(cx, |d, cx| d.set_base_text(index_text, cx));
            }
        }
    }

    /// Unregister a buffer.
    pub fn unregister_buffer(&mut self, buffer_id: BufferId) {
        self.buffer_states.remove(&buffer_id);
    }

    /// Get the BufferGitState for a buffer.
    pub fn buffer_state(&self, buffer_id: BufferId) -> Option<&BufferGitState> {
        self.buffer_states.get(&buffer_id)
    }

    /// Get a mutable reference to the BufferGitState for a buffer.
    pub fn buffer_state_mut(&mut self, buffer_id: BufferId) -> Option<&mut BufferGitState> {
        self.buffer_states.get_mut(&buffer_id)
    }

    /// Get the uncommitted diff snapshot for a buffer.
    pub fn uncommitted_diff_snapshot(
        &self,
        buffer_id: BufferId,
        cx: &App,
    ) -> Option<BufferDiffSnapshot> {
        self.buffer_states
            .get(&buffer_id)?
            .uncommitted_diff
            .upgrade()
            .map(|diff| diff.read(cx).snapshot())
    }

    /// Get the unstaged diff snapshot for a buffer.
    pub fn unstaged_diff_snapshot(
        &self,
        buffer_id: BufferId,
        cx: &App,
    ) -> Option<BufferDiffSnapshot> {
        self.buffer_states
            .get(&buffer_id)?
            .unstaged_diff
            .upgrade()
            .map(|diff| diff.read(cx).snapshot())
    }

    // -- Hunk staging / unstaging / revert ----------------------------------

    /// Stage specific hunks from a buffer's unstaged diff.
    ///
    /// This computes the new index text, writes it to the git index via
    /// the Repository entity, and uses the operation count to prevent
    /// stale reads from overwriting the in-flight write.
    pub fn stage_buffer_hunks(
        &mut self,
        buffer_id: BufferId,
        hunk_indices: &[usize],
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self.buffer_states.get(&buffer_id) else {
            return;
        };

        let new_index_text = state
            .unstaged_diff
            .upgrade()
            .and_then(|diff| diff.read(cx).stage_hunks(hunk_indices));

        let Some(new_index_text) = new_index_text else {
            return;
        };

        self.apply_hunk_index_update(buffer_id, new_index_text, cx);
    }

    /// Unstage specific hunks from a buffer's uncommitted diff.
    pub fn unstage_buffer_hunks(
        &mut self,
        buffer_id: BufferId,
        hunk_indices: &[usize],
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self.buffer_states.get(&buffer_id) else {
            return;
        };

        let new_index_text = state
            .uncommitted_diff
            .upgrade()
            .and_then(|diff| diff.read(cx).unstage_hunks(hunk_indices));

        let Some(new_index_text) = new_index_text else {
            return;
        };

        self.apply_hunk_index_update(buffer_id, new_index_text, cx);
    }

    /// Shared logic for stage/unstage: begin operation, update diff base text,
    /// write to repo, and mark write completed.
    fn apply_hunk_index_update(
        &mut self,
        buffer_id: BufferId,
        new_index_text: String,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self.buffer_states.get_mut(&buffer_id) else {
            return;
        };

        state.begin_hunk_staging_operation();
        let repo_path = state.repo_path.clone();

        // Update the unstaged diff base text immediately (optimistic).
        if let Some(diff) = state.unstaged_diff.upgrade() {
            diff.update(cx, |d, cx| {
                d.set_base_text(Some(new_index_text.clone()), cx);
            });
        }

        // Write to git index via the Repository entity.
        if let Some(repo_entity) = self.active_repository().cloned() {
            repo_entity.update(cx, |repo, _cx| {
                repo.set_index_text(repo_path, Some(new_index_text.clone()));
            });
        }

        // Mark write completed.
        if let Some(state) = self.buffer_states.get_mut(&buffer_id) {
            state.staging_write_completed(Some(new_index_text));
        }
    }

    /// Revert a hunk — replace buffer text with base text for the given
    /// hunk index. This is a pure buffer edit (no git index write).
    ///
    /// Returns the replacement text if successful (caller applies the edit).
    pub fn revert_hunk_text(
        &self,
        buffer_id: BufferId,
        hunk_index: usize,
        cx: &App,
    ) -> Option<RevertInfo> {
        let state = self.buffer_states.get(&buffer_id)?;
        let diff = state.uncommitted_diff.upgrade()?;
        let snapshot = diff.read(cx).snapshot();
        let hunks = snapshot.internal_hunks();
        let hunk = hunks.get(hunk_index)?;
        let base_text = snapshot.base_text.as_deref()?;

        Some(RevertInfo {
            buffer_line_range: hunk.buffer_range.clone(),
            replacement_text: base_text[hunk.diff_base_byte_range.clone()].to_string(),
        })
    }

    fn read_base_texts_from_repo(
        &self,
        repo_path: &RepoPath,
        cx: &mut Context<Self>,
    ) -> (Option<String>, Option<String>) {
        let Some(repo_entity) = self.active_repository() else {
            return (None, None);
        };

        let repo = repo_entity.read(cx);
        let router = repo.router();
        let local = router.local();

        let head = local.head_text_for_path(repo_path).ok().flatten();
        let index = local.index_text_for_path(repo_path).ok().flatten();

        (head, index)
    }
}
