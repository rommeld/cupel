# Zed Git Crates: Deep Architecture Analysis
### `crates/git` · `crates/project` (GitStore) · `crates/buffer_diff`

> **Purpose of this document:** A thorough implementation reference for the three supporting crates that underpin the Git Panel. It covers every major type, trait, data structure, and design decision, with rationale and Rust code skeletons oriented toward replicating the pattern in a private GPUI project.

---

## How the Four Crates Relate

```
┌─────────────────────────────────────────────────────────────┐
│  crates/git_ui   (GitPanel, ProjectDiff, ConflictView)       │
│  → reads Entity<GitStore> for all state                      │
│  → calls repo.update(cx, |r,cx| r.stage_entries(..))        │
└─────────────────────────────┬───────────────────────────────┘
                              │ Entity<GitStore>
┌─────────────────────────────▼───────────────────────────────┐
│  crates/project  (GitStore, Repository, BufferDiffStore)     │
│  → owns HashMap<RepositoryId, Entity<Repository>>           │
│  → creates/destroys Repository entities on worktree events  │
│  → owns HashMap<BufferId, (Entity<BufferDiff>,...)>          │
└───────────────┬─────────────────────────┬───────────────────┘
                │ Arc<dyn GitRepository>   │ Entity<Buffer>
┌───────────────▼──────────┐  ┌───────────▼───────────────────┐
│  crates/git              │  │  crates/buffer_diff            │
│  GitRepository trait     │  │  BufferDiff  (Entity<T>)       │
│  RealGitRepository (git2)│  │  BufferDiffSnapshot (immutable)│
│  FakeGitRepository (test)│  │  DiffHunk, SecondaryHunkStatus │
│  StatusEntry, BlameEntry │  │  imara-diff algorithm          │
└──────────────────────────┘  └───────────────────────────────┘
```

The dependency graph is strictly layered:

- `git` knows nothing about GPUI, Project, or the UI.
- `buffer_diff` knows about `language::Buffer` but not about `git` ops.
- `project/git_store` assembles both: it holds `Arc<dyn GitRepository>` for
  pure git operations and `Entity<BufferDiff>` for each open buffer's diff.
- `git_ui` only talks to `GitStore` and `Repository` through GPUI entities
  — it never calls `git2` directly.

This layering is the foundational design decision that makes the whole
system testable, remotable, and replaceable.

---

## Part 1 — `crates/git`: The Pure Git Abstraction Layer

### 1.1  What this crate is

`crates/git` is a **thin, synchronous, trait-based wrapper** around both
the `git2` Rust bindings to libgit2 and the `git` CLI binary (used for
operations libgit2 can't do, such as complex push/pull authentication
flows). It has no async runtime, no GPUI types, and no side effects beyond
touching the filesystem/index.

The crate's entire surface is the `GitRepository` trait. Everything else —
status entries, blame entries, branch info — is a data type.

### 1.2  The `GitRepository` Trait (crates/git/src/repository.rs)

```rust
pub trait GitRepository: Send + Sync {
    // ── Identity ───────────────────────────────────────────
    fn path(&self) -> &Path;               // .git directory path
    fn work_directory(&self) -> Option<&Path>;

    // ── Staging / Index ────────────────────────────────────
    fn stage_paths(
        &self,
        paths: &[RepoPath],
        env: &HashMap<String, String>,
    ) -> Result<()>;

    fn unstage_paths(
        &self,
        paths: &[RepoPath],
        env: &HashMap<String, String>,
    ) -> Result<()>;

    /// Write arbitrary text directly to the git index for one path.
    /// This is how hunk-level staging works: the caller computes
    /// the desired index content and writes it, bypassing `git add`.
    fn set_index_text(
        &self,
        path: &RepoPath,
        content: Option<String>,    // None = remove from index
        env: &HashMap<String, String>,
    ) -> Result<()>;

    fn reload_index(&self);

    // ── Commit ─────────────────────────────────────────────
    fn commit(
        &self,
        message: &str,
        options: &CommitOptions,
        env: &HashMap<String, String>,
    ) -> Result<()>;

    fn uncommit(&self, env: &HashMap<String, String>) -> Result<()>;
    // uncommit = git reset HEAD^ --soft

    // ── Remote operations ──────────────────────────────────
    fn push(
        &self,
        branch: &str,
        remote: Option<&str>,
        options: &PushOptions,
        env: &HashMap<String, String>,
    ) -> Result<()>;

    fn pull(
        &self,
        rebase: bool,
        env: &HashMap<String, String>,
    ) -> Result<()>;

    fn fetch(&self, env: &HashMap<String, String>) -> Result<()>;

    fn create_remote(&self, name: &str, url: &str) -> Result<()>;

    // ── Status ─────────────────────────────────────────────
    fn status(&self, path_prefixes: &[RepoPath]) -> Result<Vec<StatusEntry>>;
    fn status_for_path(&self, path: &RepoPath) -> Result<Option<StatusEntry>>;

    // ── Branch operations ──────────────────────────────────
    fn current_branch(&self) -> Option<Branch>;
    fn branches(&self) -> Result<Vec<Branch>>;
    fn create_branch(&self, name: &str) -> Result<()>;
    fn checkout(&self, target: &str, env: &HashMap<String, String>) -> Result<()>;
    fn delete_branch(&self, name: &str) -> Result<()>;
    fn merge_base(&self, a: &str, b: &str) -> Result<Option<String>>;
    fn remote_url(&self, name: &str) -> Option<String>;

    // ── Diff / Content ─────────────────────────────────────
    /// Read a file from HEAD (for diff base text).
    fn head_text_for_path(&self, path: &RepoPath) -> Result<Option<String>>;

    /// Read a file from the index (for staging status diff base).
    fn index_text_for_path(&self, path: &RepoPath) -> Result<Option<String>>;

    // ── Blame ──────────────────────────────────────────────
    fn blame_for_path(
        &self,
        path: &RepoPath,
        content: Rope,
    ) -> Result<Vec<BlameEntry>>;

    // ── Stash ──────────────────────────────────────────────
    fn stash_list(&self) -> Result<Vec<StashEntry>>;
    fn stash_all(&self, message: Option<&str>) -> Result<()>;
    fn stash_pop(&self, index: usize) -> Result<()>;
    fn stash_apply(&self, index: usize) -> Result<()>;
    fn stash_drop(&self, index: usize) -> Result<()>;

    // ── History ────────────────────────────────────────────
    fn log(&self, path: Option<&RepoPath>, limit: usize) -> Result<Vec<CommitSummary>>;
    fn show(&self, oid: &str) -> Result<CommitDetails>;

    // ── Conflict resolution ────────────────────────────────
    fn checkout_conflict_path(&self, path: &RepoPath, side: ConflictSide) -> Result<()>;
}
```

**Key design decisions:**

1. **All methods are synchronous.** The async wrapper is in `project/git_store.rs`, not here. This makes the trait trivially testable.
2. **`env: &HashMap<String, String>` parameter.** Every mutating operation receives a git environment map. This is how the AskPass credential helper is injected — the map contains `GIT_ASKPASS=/path/to/helper` and a unique delegate ID. The implementation forwards this into the git2 process environment or into the CLI call.
3. **Dual backend.** Pure metadata operations (status, blame, log, index text) use the `git2` library for speed; network operations (push, pull, fetch) shell out to the `git` CLI because libgit2's HTTP/SSH authentication does not support the same credential helper ecosystem as the CLI.

### 1.3  Data Types

#### `RepoPath`

```rust
/// A path relative to the repository root (not the worktree root).
/// Implements `Ord` for use in `SumTree`.
pub struct RepoPath(Arc<Path>);
```

`RepoPath` is the canonical identifier for a file within a repository. It is **not** a `ProjectPath` (which includes a `WorktreeId`) and **not** an absolute path. The conversion between them is handled in `git_store.rs`.

#### `StatusEntry`

```rust
pub struct StatusEntry {
    pub repo_path: RepoPath,
    pub status: GitFileStatus,
}

pub struct GitFileStatus {
    pub index_status: FileStatus,     // staged state (vs HEAD)
    pub worktree_status: FileStatus,  // unstaged state (vs index)
    pub conflict: bool,               // unmerged paths
}

pub enum FileStatus {
    Added,
    Modified,
    Deleted,
    Untracked,
    Unchanged,
}
```

`StatusEntry` is the atomic unit of git status. A single file entry encodes **both** its staged and unstaged state simultaneously — this is what enables the three-state checkbox in the UI without needing two separate lists.

The staging state of a file is derived by comparing `index_status` and `worktree_status`:

```
index_status == Unchanged && worktree_status != Unchanged  → fully unstaged
index_status != Unchanged && worktree_status == Unchanged  → fully staged
both non-Unchanged                                          → partially staged
```

#### `BlameEntry`

```rust
pub struct BlameEntry {
    pub sha: Oid,               // commit SHA
    pub line_range: Range<u32>, // 0-based line range this commit owns
    pub author: Option<String>,
    pub author_mail: Option<String>,
    pub author_time: Option<DateTime<Utc>>,
    pub committer: Option<String>,
    pub summary: Option<String>,
}
```

Blame entries are stored in a `SumTree<BlameEntry>` keyed by line range, enabling O(log N) lookup of "which commit owns line N".

#### `Branch`

```rust
pub struct Branch {
    pub name: SharedString,
    pub upstream: Option<UpstreamBranch>,
    pub is_head: bool,
    pub unix_timestamp: Option<i64>,  // for sorting by recency
}

pub struct UpstreamBranch {
    pub name: SharedString,
    pub ahead: Option<u32>,
    pub behind: Option<u32>,
}
```

Ahead/behind counts power the push/pull badge displayed in the GitPanel toolbar.

#### `CommitOptions`

```rust
pub struct CommitOptions {
    pub amend: bool,
    pub signoff: bool,
}
```

Passed directly to `GitRepository::commit()`. The UI toggle maps directly to these fields.

### 1.4  `RealGitRepository` — the git2 implementation

```rust
pub struct RealGitRepository {
    // The libgit2 repository handle — NOT Send by default;
    // we wrap it in a Mutex to allow use from background threads.
    repository: Mutex<git2::Repository>,
    // Cached path so we don't lock for read-only queries.
    path: PathBuf,
    work_directory: Option<PathBuf>,
}
```

**Critical implementation note:** `git2::Repository` is not `Sync`, so Zed
wraps it in a `Mutex`. The background job worker (in `git_store.rs`) holds
the `Arc<dyn GitRepository>` and the `Mutex` is locked for the duration of
each operation. This is safe because all git operations are serialized
through the job queue anyway.

**Status reading** uses `git2::Repository::statuses()` with
`StatusOptions::include_untracked(true)` and maps the `git2::Status` bitmask
into Zed's `GitFileStatus` struct.

**Index writes** for `set_index_text()` use `git2::Repository::index()` to get
the in-memory index, then:

```rust
// For set_index_text(path, Some(content)):
let oid = repo.blob(content.as_bytes())?;
let entry = IndexEntry { path: ..., id: oid, mode: 0o100644, ... };
index.add(&entry)?;
index.write()?;

// For set_index_text(path, None):
index.remove_path(path)?;
index.write()?;
```

After writing, `reload_index()` is called, which calls `index.read(true)` to
reload from disk (necessary because other tools may write the index).

**Push/pull/fetch** are implemented by spawning a `git` process:

```rust
Command::new("git")
    .args(["push", remote, branch])
    .envs(env)          // includes GIT_ASKPASS
    .current_dir(&self.work_directory)
    .spawn()?
```

The `env` map passed from `git_store.rs` contains at minimum:
- `GIT_ASKPASS` — path to the Zed askpass helper binary
- `ZEDASKPASS_DELEGATE_ID` — unique integer identifying which `AskPassDelegate` in `GitStore` receives the credential prompt

### 1.5  `FakeGitRepository` — the test double

```rust
pub struct FakeGitRepository {
    // Head commit: path → content
    head: HashMap<RepoPath, String>,
    // Index: path → content (staged state)
    index: HashMap<RepoPath, String>,
    // Working tree: path → content (read from FakeFs)
    fs: Arc<FakeFs>,
    work_directory: PathBuf,
}
```

`FakeGitRepository` derives status by comparing `head`, `index`, and `fs`
contents — it does not call any git binary. After the PR
[#26961](https://github.com/zed-industries/zed/pull/26961) it generates
realistic status: if you modify a file in `FakeFs`, `status()` returns
`Modified`. This allows integration tests to exercise the full
`GitStore → Repository → Panel` reactive chain without a real git repository.

To use it in tests:

```rust
let repo = FakeGitRepository::new(fs.clone(), root_path);
repo.set_head_content(RepoPath::from("src/main.rs"), "fn main() {}");
// Modify the file in the fake filesystem...
fs.set_content(root.join("src/main.rs"), "fn main() { println!(\"hi\"); }").await;
// Now repo.status() will return Modified for src/main.rs
```

### 1.6  `crates/git/src/status.rs` — SumTree integration

`StatusEntry` implements the `sum_tree::Item` trait, which requires a
`Summary` type and a `key()` method:

```rust
impl sum_tree::Item for StatusEntry {
    type Summary = PathSummary;
    fn summary(&self) -> Self::Summary {
        PathSummary {
            max_path: self.repo_path.clone(),
            count: 1,
        }
    }
}

#[derive(Clone, Default)]
pub struct PathSummary {
    pub max_path: RepoPath,
    pub count: usize,
}

impl sum_tree::Summary for PathSummary {
    type Context = ();
    fn add_summary(&mut self, other: &Self, _: &()) {
        if other.max_path > self.max_path {
            self.max_path = other.max_path.clone();
        }
        self.count += other.count;
    }
}
```

This enables `SumTree<StatusEntry>` to support:
- O(log N) lookup by `RepoPath` using `tree.get(&path, &())`
- Total count in O(1) from the root summary
- Ordered iteration for building the flat list

---

## Part 2 — `crates/project`: GitStore and Repository Entities

`git_store.rs` is the largest single file in the git stack (~2,400 lines). It
translates raw `GitRepository` operations into a GPUI reactive system:
entities, subscriptions, async jobs, events, and protobuf RPC.

### 2.1  `GitStore` struct

```rust
pub struct GitStore {
    // ── Mode ───────────────────────────────────────────────────────
    mode: GitStoreMode,

    // ── Repository registry ────────────────────────────────────────
    repositories: HashMap<RepositoryId, Entity<Repository>>,
    active_repository: Option<Entity<Repository>>,
    next_repository_id: RepositoryId,

    // ── Buffer diff registry ───────────────────────────────────────
    /// For each open buffer in a git repo, we track two diffs:
    /// (unstaged_diff, uncommitted_diff).
    /// unstaged:    working tree vs. index     (base = index text)
    /// uncommitted: working tree vs. HEAD      (base = HEAD text)
    buffer_diffs: HashMap<BufferId,
        (Option<Entity<BufferDiff>>, Option<Entity<BufferDiff>>)>,

    // ── Shared state ───────────────────────────────────────────────
    worktree_store: Entity<WorktreeStore>,
    buffer_store: Entity<BufferStore>,
    project_path_for_repo_path: HashMap<RepositoryId,
        Box<dyn Fn(&RepoPath) -> Option<ProjectPath>>>,

    // ── AskPass credential infrastructure ─────────────────────────
    askpass_delegates: HashMap<u64, AskPassDelegate>,
    next_askpass_id: u64,

    // ── Serialization ──────────────────────────────────────────────
    _subscriptions: Vec<Subscription>,
}

enum GitStoreMode {
    /// Local project: we own real Repository entities.
    Local,
    /// Remote project: operations forwarded to host via RPC.
    Remote { upstream: AnyProtoClient },
}
```

#### `RepositoryId`

```rust
pub struct RepositoryId(u64);
```

A monotonically incrementing integer assigned when `GitStore` creates a new
`Repository` entity. It is stable for the lifetime of the project session
(but not persisted across restarts).

#### How `GitStore` is created

`GitStore` is constructed inside `Project::local()`:

```rust
let git_store = cx.new(|cx| {
    GitStore::local(worktree_store.clone(), buffer_store.clone(), cx)
});
```

In `GitStore::local()`, the constructor subscribes to `WorktreeStore`:

```rust
let worktree_sub = cx.subscribe(&worktree_store, |this, _, event, cx| {
    match event {
        WorktreeStoreEvent::WorktreeUpdatedGitRepositories(worktree_id, repos) => {
            this.update_repositories(worktree_id, repos, cx);
        }
        _ => {}
    }
});

let buffer_sub = cx.subscribe(&buffer_store, |this, _, event, cx| {
    match event {
        BufferStoreEvent::BufferOpened(buffer) => {
            this.create_buffer_diffs_for(&buffer, cx);
        }
        BufferStoreEvent::BufferClosed(buffer_id) => {
            this.buffer_diffs.remove(&buffer_id);
        }
    }
});

self._subscriptions = vec![worktree_sub, buffer_sub];
```

### 2.2  `GitStoreEvent` — the event type

```rust
pub enum GitStoreEvent {
    /// A Repository entity was added or its active branch changed.
    ActiveRepositoryChanged,
    /// Repository status entries changed (files staged/unstaged/modified).
    StatusesChanged(RepositoryId),
    /// A commit completed.
    CommitCompleted { repository_id: RepositoryId, was_amend: bool },
    /// Repository HEAD moved (branch switch, commit, uncommit).
    BranchChanged,
    /// Buffer diffs were recalculated.
    DiffChanged { buffer_id: BufferId },
}
```

The `GitPanel` subscribes to these events in its constructor. The pattern is:

```rust
let _git_sub = cx.subscribe(&git_store, |this, _store, event, cx| {
    match event {
        GitStoreEvent::StatusesChanged(_) => this.rebuild_entries(cx),
        GitStoreEvent::ActiveRepositoryChanged => {
            this.selected_ix = None;
            this.rebuild_entries(cx);
        }
        GitStoreEvent::CommitCompleted { .. } => {
            this.commit_editor.update(cx, |ed, cx| ed.set_text("", cx));
        }
        GitStoreEvent::BranchChanged => cx.notify(),
        _ => {}
    }
});
```

### 2.3  Repository Discovery

When the worktree scanner discovers a `.git` directory, it reports it via
`WorktreeStoreEvent::WorktreeUpdatedGitRepositories`. `GitStore` then calls:

```rust
fn update_repositories(
    &mut self,
    worktree_id: WorktreeId,
    discovered: Vec<RepositoryEntry>,  // from WorktreeSnapshot
    cx: &mut Context<Self>,
) {
    for entry in discovered {
        if self.repositories.values().any(|r| r.read(cx).dot_git_path == entry.work_directory) {
            // Already known; refresh its state.
            continue;
        }
        // Create a new Repository entity.
        let repo_entity = cx.new(|cx| {
            Repository::new(entry, worktree_id, cx)
        });

        let id = self.next_repository_id.post_increment();
        self.repositories.insert(id, repo_entity.clone());

        // When the Repository entity changes, re-emit a GitStoreEvent.
        let sub = cx.observe(&repo_entity, |this, _, cx| {
            cx.emit(GitStoreEvent::StatusesChanged(id));
        });
        self._subscriptions.push(sub);

        // If this is the first repository or matches the active worktree,
        // set it as active.
        if self.active_repository.is_none() {
            self.active_repository = Some(repo_entity);
            cx.emit(GitStoreEvent::ActiveRepositoryChanged);
        }
    }
}
```

The path-conversion closure `project_path_for_repo_path` is also set here:

```rust
self.project_path_for_repo_path.insert(id, Box::new(move |repo_path| {
    // Map repo-relative path → (worktree_id, worktree-relative path)
    let worktree_relative = repo_root.join(repo_path).strip_prefix(&worktree_root)?;
    Some(ProjectPath { worktree_id, path: Arc::from(worktree_relative) })
}));
```

This closure is called by the `GitPanel` when the user opens a file from the
changed-files list.

### 2.4  `Repository` entity — per-repository state

```rust
pub struct Repository {
    // ── Identity ───────────────────────────────────────────────────
    pub worktree_id: WorktreeId,
    pub dot_git_path: PathBuf,      // abs path to .git dir

    // ── Status ground truth ────────────────────────────────────────
    pub statuses: SumTree<StatusEntry>,
    pub snapshot: RepositorySnapshot,

    // ── Git backend ────────────────────────────────────────────────
    git_repo: Arc<dyn GitRepository>,

    // ── Async job queue ────────────────────────────────────────────
    job_sender: mpsc::UnboundedSender<GitJob>,
    active_jobs: HashMap<JobId, Task<()>>,  // tasks kept alive
    next_job_id: JobId,

    // ── Optimistic state for the UI ────────────────────────────────
    /// Paths with pending staging/unstaging operations.
    /// The panel shows these as if already staged/unstaged.
    pending_ops: HashMap<RepoPath, PendingOp>,

    // ── Commit editor state ────────────────────────────────────────
    commit_message_buffer: Option<Entity<Buffer>>,

    // ── Upstream tracking (for push/pull badges) ───────────────────
    push_counts: Option<AheadBehind>,   // commits ahead of remote
    pull_counts: Option<AheadBehind>,   // commits behind remote
}

#[derive(Clone)]
pub struct RepositorySnapshot {
    pub branch: Option<Branch>,
    pub merge_details: Option<MergeDetails>,
    pub stash_count: usize,
}

pub struct MergeDetails {
    pub conflicted_paths: TreeSet<RepoPath>,
    pub message: Option<SharedString>,     // from .git/MERGE_MSG
    pub heads: Vec<Option<SharedString>>,  // merge head SHAs
}
```

#### `RepositorySnapshot` vs `Repository`

`RepositorySnapshot` is a cheap-to-clone value type that the `GitPanel`
reads on every render without locking anything:

```rust
// In GitPanel::render_toolbar():
let snapshot = repo.read(cx).snapshot.clone();  // cheap Arc clone
let branch_name = snapshot.branch.as_ref().map(|b| b.name.clone());
```

The full `Repository` struct with its `SumTree<StatusEntry>` is only touched
through `Entity::update()`.

### 2.5  The Async Job Queue Pattern

This pattern is one of the most important in the whole codebase for building
responsive, correct UIs over slow I/O.

```rust
enum GitJob {
    RefreshStatus { path_prefixes: Vec<RepoPath> },
    StageEntries  { paths: Vec<RepoPath>, tx: oneshot::Sender<Result<()>> },
    UnstageEntries{ paths: Vec<RepoPath>, tx: oneshot::Sender<Result<()>> },
    SetIndexText  { path: RepoPath, content: Option<String>, tx: oneshot::Sender<Result<()>> },
    Commit        { message: String, options: CommitOptions, tx: oneshot::Sender<Result<()>> },
    Push          { remote: Option<String>, tx: oneshot::Sender<Result<()>> },
    Pull          { rebase: bool, tx: oneshot::Sender<Result<()>> },
    Fetch         { tx: oneshot::Sender<Result<()>> },
    // ...
}
```

**Queue construction in `Repository::new()`:**

```rust
let (job_sender, mut job_receiver) = mpsc::unbounded_channel::<GitJob>();

let git_repo_clone = git_repo.clone();
let executor = cx.background_executor().clone();
let entity = cx.weak_entity();

// This task runs for the entire lifetime of the Repository entity.
// It serializes all git operations onto a single background thread.
let worker_task = cx.background_spawn(async move {
    while let Some(job) = job_receiver.next().await {
        match job {
            GitJob::RefreshStatus { path_prefixes } => {
                let result = git_repo_clone.status(&path_prefixes);
                entity.update(&mut executor.clone(), |repo, cx| {
                    if let Ok(entries) = result {
                        repo.statuses = SumTree::from_iter(entries, &());
                        cx.notify();   // ← triggers re-render
                    }
                }).ok();
            }
            GitJob::StageEntries { paths, tx } => {
                let env = build_askpass_env();
                let result = git_repo_clone.stage_paths(&paths, &env);
                let _ = tx.send(result);
                // After staging, always refresh status:
                let _ = git_repo_clone.status(&[]);
                entity.update(/* update statuses and cx.notify() */).ok();
            }
            // ... other arms
        }
    }
});
// Store the task so it stays alive:
self._worker_task = Some(worker_task);
```

**Key invariants:**
- Jobs are processed one at a time (single background thread).
- Status is always refreshed after any mutating operation.
- The `oneshot::Sender<Result<()>>` in each mutating job lets the caller
  await the result if needed (e.g. to show an error toast).
- `entity.update(...)` safely re-enters the GPUI entity from the background
  thread via `WeakEntity::update`.

**Submitting a job from the panel:**

```rust
// In GitPanel, e.g. on_stage_all():
self.git_store.update(cx, |store, cx| {
    if let Some(repo) = store.active_repository() {
        repo.update(cx, |repo, _cx| {
            let (tx, rx) = oneshot::channel();
            repo.job_sender.send(GitJob::StageEntries {
                paths: selected_paths,
                tx,
            }).ok();
            // Optionally await rx in a spawned task for error reporting.
        });
    }
});
```

### 2.6  Optimistic UI with `pending_ops`

One subtle refinement: when the user clicks "Stage", the status list should
update **immediately** without waiting for the background job to complete and
the status to be refreshed (which can take ~50–200ms on large repos).

```rust
pub enum PendingOp {
    Staging,    // file is being staged
    Unstaging,  // file is being unstaged
}
```

Before sending the job, `Repository` records the pending state:

```rust
fn stage_entries(&mut self, paths: Vec<RepoPath>, cx: &mut Context<Self>) {
    for path in &paths {
        self.pending_ops.insert(path.clone(), PendingOp::Staging);
    }
    cx.notify(); // render with optimistic state immediately

    let (tx, rx) = oneshot::channel();
    self.job_sender.send(GitJob::StageEntries { paths: paths.clone(), tx }).ok();

    // When the job completes, clear pending_ops and notify again:
    let entity = cx.entity().downgrade();
    cx.spawn(|_window, cx| async move {
        if rx.await.is_ok() {
            entity.update(cx, |repo, cx| {
                for path in &paths { repo.pending_ops.remove(path); }
                cx.notify();
            }).ok();
        }
    }).detach();
}
```

The `GitPanel::render_entry()` checks `pending_ops` to show a spinner or
tentative checkbox state:

```rust
let effective_staged = if repo.pending_ops.contains_key(&entry.repo_path) {
    PendingOp::Staging => CheckboxState::Checked,    // show as staged
    PendingOp::Unstaging => CheckboxState::Unchecked, // show as unstaged
} else {
    entry.staging_state()  // derived from GitFileStatus
};
```

### 2.7  Buffer Diff Lifecycle in `GitStore`

```rust
fn create_buffer_diffs_for(
    &mut self,
    buffer: &Entity<Buffer>,
    cx: &mut Context<Self>,
) {
    let buffer_id = buffer.read(cx).remote_id();
    let path = buffer.read(cx).file().map(|f| f.path().clone())?;

    // Find which repository owns this path.
    let (repo_id, repo) = self.repository_for_path(&path, cx)?;
    let git_repo = repo.read(cx).git_repo.clone();

    // --- Create the UNSTAGED diff (working tree vs. index) ---
    let unstaged_diff = cx.new(|cx| {
        let index_text = git_repo.index_text_for_path(&repo_path_from(&path)).ok().flatten();
        BufferDiff::new(buffer.clone(), index_text, cx)
    });

    // --- Create the UNCOMMITTED diff (working tree vs. HEAD) ---
    let uncommitted_diff = cx.new(|cx| {
        let head_text = git_repo.head_text_for_path(&repo_path_from(&path)).ok().flatten();
        BufferDiff::new(buffer.clone(), head_text, cx)
    });

    self.buffer_diffs.insert(buffer_id, (
        Some(unstaged_diff),
        Some(uncommitted_diff),
    ));
}
```

When the index is updated (e.g. after staging), the unstaged diff needs to
recompute its base text. `GitStore` handles this by observing staging
completion:

```rust
// In Repository's job worker, after a successful StageEntries:
entity.update(cx, |repo, cx| {
    cx.emit(RepositoryEvent::IndexChanged);
});

// In GitStore, subscribed to RepositoryEvent:
cx.subscribe(&repo, |this, _, event, cx| {
    if matches!(event, RepositoryEvent::IndexChanged) {
        this.refresh_unstaged_diffs_for(repo_id, cx);
    }
});
```

### 2.8  The AskPass Credential System

Remote operations require a way to prompt the user for credentials without
blocking the main thread. Zed implements this with a custom askpass helper.

```
git push (subprocess)
    ↓
GIT_ASKPASS=/path/to/zed-askpass
    ↓
zed-askpass binary (tiny C program, just calls a Unix socket)
    ↓
Unix socket ← ZEDASKPASS_SOCKET_PATH env var
    ↓
Zed main process receives credential request
    ↓
GitStore looks up AskPassDelegate by ZEDASKPASS_DELEGATE_ID
    ↓
AskPassDelegate sends prompt to UI layer (shows modal)
    ↓
User types password → sent back through the socket
    ↓
zed-askpass receives the password string → writes to stdout
    ↓
git receives the credential
```

In `GitStore`:

```rust
pub struct AskPassDelegate {
    /// Called when git needs a prompt answered.
    callback: Box<dyn FnOnce(String, oneshot::Sender<String>)>,
}

fn create_askpass_env(&mut self, cx: &mut Context<Self>) -> HashMap<String, String> {
    let id = self.next_askpass_id;
    self.next_askpass_id += 1;

    let (prompt_tx, mut prompt_rx) = mpsc::channel(1);
    let entity = cx.entity().downgrade();

    // The UI registers a delegate that shows a modal:
    self.askpass_delegates.insert(id, AskPassDelegate {
        callback: Box::new(move |prompt, response_tx| {
            entity.update(/* show credential modal, write to response_tx */).ok();
        }),
    });

    let mut env = HashMap::new();
    env.insert("GIT_ASKPASS".into(), askpass_binary_path());
    env.insert("ZEDASKPASS_DELEGATE_ID".into(), id.to_string());
    env.insert("ZEDASKPASS_SOCKET_PATH".into(), socket_path().to_string_lossy().into());
    env
}
```

**If you are implementing a similar pattern:** You don't need the full askpass
infrastructure for a private project unless you're doing remote git
operations. For local operations (stage, commit, status), the `env` map can
be empty or contain only user-level git config overrides.

### 2.9  Remote Mode (`GitStoreMode::Remote`)

In SSH remote development, the **client** machine has a `GitStore` in
`Remote` mode that caches snapshots for display but routes all write
operations to the **host** via protobuf RPC.

```rust
// On the client side (GitStore in Remote mode):
fn stage_entries(&self, paths: Vec<RepoPath>, cx: &mut Context<Self>) {
    match &self.mode {
        GitStoreMode::Local => { /* submit job to Repository */ }
        GitStoreMode::Remote { upstream } => {
            upstream.request(proto::StageEntries {
                project_id: REMOTE_SERVER_PROJECT_ID,
                paths: paths.iter().map(|p| p.to_proto()).collect(),
            });
        }
    }
}

// The host's GitStore registers RPC handlers:
fn shared(&mut self, project_id: u64, client: AnyProtoClient, cx: &mut Context<Self>) {
    client.add_request_handler(cx.entity(), Self::handle_stage_entries);
    client.add_request_handler(cx.entity(), Self::handle_unstage_entries);
    // ...
}

async fn handle_stage_entries(
    this: Entity<GitStore>,
    envelope: TypedEnvelope<proto::StageEntries>,
    cx: AsyncApp,
) -> Result<proto::Ack> {
    this.update(cx, |store, cx| {
        // Runs the local job queue path
        store.stage_entries_local(paths, cx);
    })?;
    Ok(proto::Ack {})
}
```

The `proto/git.proto` file defines the full RPC surface:

```protobuf
message StageEntries { uint64 project_id = 1; repeated string paths = 2; }
message UnstageEntries { uint64 project_id = 1; repeated string paths = 2; }
message SetIndexText { uint64 project_id = 1; string path = 2; optional string content = 3; }
message Commit { uint64 project_id = 1; string message = 2; bool amend = 3; bool signoff = 4; }
message UpdateRepository { /* full repository snapshot for client cache */ }
```

The `UpdateRepository` message is sent from host to client whenever a
`Repository` entity's status changes, updating the client's cached display
state without the client needing to run `git status` itself.

---

## Part 3 — `crates/buffer_diff`: Per-Buffer Diff State

`buffer_diff` is a focused crate with one job: given a `Buffer` entity and
a base text string, maintain an up-to-date set of diff hunks between the
current buffer content and the base text.

### 3.1  Type Hierarchy

```
BufferDiff  (Entity<BufferDiff>, stateful, has subscriptions)
    └─ snapshot: BufferDiffSnapshot

BufferDiffSnapshot  (cheap Clone, immutable value type)
    └─ hunks: Vec<DiffHunk>
    └─ base_text: Option<String>

DiffHunk
    └─ buffer_range: Range<Anchor>    // positions in the current buffer
    └─ diff_base_byte_range: Range<usize>  // byte offset in base_text
    └─ secondary_status: SecondaryHunkStatus
```

This pair (`BufferDiff` resource / `BufferDiffSnapshot` value) mirrors
the `Buffer` / `BufferSnapshot` pattern throughout the codebase, and the
`SumTree` / snapshot pattern from the Rope.

### 3.2  `BufferDiff` entity

```rust
pub struct BufferDiff {
    buffer: Entity<Buffer>,
    snapshot: BufferDiffSnapshot,

    // The background task that recomputes diffs.
    diff_task: Option<Task<()>>,

    // Optional secondary diff: another BufferDiff whose hunks
    // are compared against ours to determine staging status.
    secondary_diff: Option<WeakEntity<BufferDiff>>,

    _subscription: Subscription, // buffer change subscription
}
```

**Construction:**

```rust
impl BufferDiff {
    pub fn new(
        buffer: Entity<Buffer>,
        base_text: Option<String>,
        cx: &mut Context<Self>,
    ) -> Self {
        let sub = cx.observe(&buffer, |this, buffer, cx| {
            this.recalculate(buffer, cx);
        });

        let mut this = Self {
            buffer: buffer.clone(),
            snapshot: BufferDiffSnapshot::new(base_text),
            diff_task: None,
            secondary_diff: None,
            _subscription: sub,
        };
        // Run initial diff:
        this.recalculate(buffer.read(cx).snapshot(), cx);
        this
    }
}
```

The `cx.observe(&buffer, ...)` subscription fires whenever `cx.notify()` is
called on the buffer — i.e., after every edit. This keeps the diff always
current with zero additional bookkeeping.

### 3.3  `DiffHunk`

```rust
pub struct DiffHunk {
    /// Range in the current buffer (Anchors survive edits).
    pub buffer_range: Range<Anchor>,

    /// Byte range in `base_text` that this hunk replaces.
    pub diff_base_byte_range: Range<usize>,

    /// Whether this hunk is staged, unstaged, or partially staged.
    pub secondary_status: SecondaryHunkStatus,
}

pub enum SecondaryHunkStatus {
    /// The hunk does not exist in the secondary diff (fully staged).
    NoSecondaryHunk,
    /// The hunk exists in the secondary diff (fully unstaged).
    HasSecondaryHunk,
    /// The hunk partially overlaps the secondary diff (partially staged).
    OverlapsWithSecondaryHunk,
}
```

**How `SecondaryHunkStatus` is computed:**

When the "unstaged diff" (working tree vs. index) has a secondary diff set
to the "uncommitted diff" (working tree vs. HEAD), each hunk in the unstaged
diff is compared against the uncommitted hunks:

```rust
fn compute_secondary_status(
    unstaged_hunk: &DiffHunk,
    uncommitted_hunks: &[DiffHunk],  // from the secondary diff
) -> SecondaryHunkStatus {
    // An uncommitted hunk covers the same buffer region:
    let overlapping: Vec<_> = uncommitted_hunks
        .iter()
        .filter(|h| ranges_overlap(&h.buffer_range, &unstaged_hunk.buffer_range))
        .collect();

    if overlapping.is_empty() {
        // No uncommitted hunk covers this range.
        // This means the unstaged diff exists but the uncommitted diff does not
        // → the change is in the index (staged) but not in HEAD → FULLY STAGED.
        SecondaryHunkStatus::NoSecondaryHunk
    } else if overlapping.iter().all(|h| h.buffer_range == unstaged_hunk.buffer_range) {
        // Perfect match: both diffs have the same range.
        // This means the change exists both as unstaged and in HEAD → FULLY UNSTAGED.
        SecondaryHunkStatus::HasSecondaryHunk
    } else {
        // Partial overlap: some lines of this hunk are in HEAD, some are not.
        SecondaryHunkStatus::OverlapsWithSecondaryHunk
    }
}
```

This is the mathematical heart of the "diff-of-diffs" algorithm. The
insight is that comparing the *positions* of hunks in two separate diffs
against HEAD naturally reveals which lines have been staged.

### 3.4  `recalculate()` — the diff algorithm

```rust
fn recalculate(
    &mut self,
    buffer: BufferSnapshot,
    cx: &mut Context<Self>,
) {
    let base_text = self.snapshot.base_text.clone();
    let secondary = self.secondary_diff.clone();

    self.diff_task = Some(cx.background_spawn(async move {
        // imara-diff is the diff engine (fast, deterministic, line-based).
        let hunks = if let Some(base) = &base_text {
            compute_diff_hunks(&buffer, base.as_str())
        } else {
            vec![]
        };

        // Read secondary diff hunks for staging status computation.
        let secondary_hunks: Vec<DiffHunk> = secondary
            .and_then(|s| s.upgrade())
            .map(|s| s.read_with(cx, |d, _| d.snapshot.hunks.clone()))
            .unwrap_or_default();

        let hunks_with_status = hunks.into_iter().map(|hunk| {
            let status = compute_secondary_status(&hunk, &secondary_hunks);
            DiffHunk { secondary_status: status, ..hunk }
        }).collect();

        hunks_with_status  // returned to main thread
    }));

    // When the task completes, update snapshot and notify:
    let task = self.diff_task.take().unwrap();
    cx.spawn(|this, cx| async move {
        let new_hunks = task.await;
        this.update(cx, |diff, cx| {
            diff.snapshot.hunks = new_hunks;
            cx.notify();
        }).ok();
    }).detach();
}
```

**The `imara-diff` crate** (replaced `similar` in 2024) provides:
- A Myers/Histogram line-level diff
- A secondary word-level diff within replaced lines (for word-diff rendering)
- Better performance on large files than `similar`

### 3.5  `BufferDiffSnapshot` — the value type

```rust
#[derive(Clone, Default)]
pub struct BufferDiffSnapshot {
    pub hunks: Vec<DiffHunk>,
    pub base_text: Option<String>,
}

impl BufferDiffSnapshot {
    /// Look up all hunks that intersect a buffer row range.
    pub fn hunks_intersecting_range(
        &self,
        range: Range<u32>,          // row range (0-based)
        buffer: &BufferSnapshot,
    ) -> impl Iterator<Item = &DiffHunk> {
        // Binary search since hunks are sorted by buffer_range start.
        // ...
    }

    /// Compute the new index text if we stage/unstage a specific hunk.
    /// Used for hunk-level staging.
    pub fn text_for_staged_hunk(
        &self,
        hunk: &DiffHunk,
        buffer: &BufferSnapshot,
        stage: bool,   // true = stage this hunk, false = unstage
    ) -> String {
        // Apply or reverse the diff hunk against base_text.
        // The result is the content that should be written to the git index.
        // ...
    }
}
```

`text_for_staged_hunk()` is the key method for hunk-level staging. The
`ToggleStaged` editor action calls this, then passes the result to
`Repository::set_index_text()`.

### 3.6  How the Editor Uses `BufferDiff`

The Editor's gutter display and inline diff rendering are driven by
`BufferDiff` through a read-only subscription pattern:

```rust
// In Editor::new(), called by GitStore:
pub fn set_buffer_diff(&mut self, diff: Entity<BufferDiff>, cx: &mut Context<Self>) {
    self._buffer_diff_sub = cx.observe(&diff, |editor, diff, cx| {
        editor.buffer_diff = Some(diff.read(cx).snapshot.clone());
        editor.request_layout(cx);  // trigger re-render of gutter
    });
    self.buffer_diff = Some(diff.read(cx).snapshot.clone());
}
```

On each render pass, the editor reads `self.buffer_diff.hunks` to paint the
colored gutter indicators (green = added, yellow = modified, red = deleted)
and to display the inline diff view when a hunk is expanded.

---

## Part 4 — The Worktree: How Repositories Are Discovered

The `Worktree` entity (in `crates/worktree`) is worth understanding briefly
because it is the source of `Repository` discovery.

### 4.1  `WorktreeSnapshot` and repository detection

When the background file scanner finds a `.git` directory, it records a
`RepositoryEntry`:

```rust
pub struct RepositoryEntry {
    /// Path of the `.git` dir (relative to the worktree root).
    pub work_directory: WorkDirectory,
    /// The current branch and status, lazily populated.
    pub branch: Option<Arc<str>>,
    /// An `Arc<dyn GitRepository>` open to this directory.
    pub git_repo: Arc<dyn GitRepository>,
}

pub enum WorkDirectory {
    /// The `.git` is inside the worktree.
    InProject { work_directory: Arc<Path> },
    /// The `.git` is outside (e.g. git submodule, git worktree).
    External { work_directory: Arc<Path> },
}
```

The `LocalWorktree` background scanner uses `git2::Repository::discover()`
on each scanned directory to find `.git` roots. When it finds one, it
constructs a `RealGitRepository` and stores it in `RepositoryEntry`.

### 4.2  `WorktreeStoreEvent::WorktreeUpdatedGitRepositories`

After each scan cycle, if any `.git` directories were added, removed, or
changed:

```rust
cx.emit(WorktreeStoreEvent::WorktreeUpdatedGitRepositories(
    worktree_id,
    updated_entries,
));
```

`GitStore` receives this and calls `update_repositories()` as shown in §2.3.

### 4.3  `Entry::git_status` in the project panel

`Entry` (the file/directory node in the project panel) carries:

```rust
pub struct Entry {
    // ...
    pub git_status: Option<GitFileStatus>,
}
```

The worktree keeps this updated by reading from the
`SumTree<StatusEntry>` inside `Repository`. The project panel's color coding
of filenames comes from this field — it reads `git_status` to choose between
`added` (green), `modified` (yellow), `untracked` (grey/dimmed), etc.

---

## Part 5 — Implementing the Pattern in Your Own Project

This section distills the four crates into a minimal viable implementation
pattern for a private GPUI application.

### 5.1  Crate Decomposition

```
your_project/
├── crates/
│   ├── my_vcs/          # Analogous to crates/git
│   │   ├── src/
│   │   │   ├── repository.rs  # trait + real impl
│   │   │   ├── status.rs      # StatusEntry + SumTree impls
│   │   │   └── types.rs       # Branch, CommitOptions, etc.
│   ├── my_project/      # Analogous to crates/project
│   │   └── src/
│   │       └── vcs_store.rs   # Entity<VcsStore>, Entity<Repo>
│   └── my_ui/           # Analogous to crates/git_ui
│       └── src/
│           └── vcs_panel.rs   # Entity<VcsPanel>, Render
```

### 5.2  Minimal `Repository` trait

```rust
pub trait Repository: Send + Sync {
    fn status(&self) -> Result<Vec<StatusEntry>>;
    fn stage(&self, paths: &[Path], env: &Env) -> Result<()>;
    fn unstage(&self, paths: &[Path], env: &Env) -> Result<()>;
    fn set_index_text(&self, path: &Path, content: Option<&str>, env: &Env) -> Result<()>;
    fn commit(&self, msg: &str, env: &Env) -> Result<()>;
    fn head_text(&self, path: &Path) -> Result<Option<String>>;
    fn index_text(&self, path: &Path) -> Result<Option<String>>;
    fn current_branch(&self) -> Option<String>;
}

pub type Env = HashMap<String, String>;
```

### 5.3  `VcsStore` entity skeleton

```rust
pub struct VcsStore {
    repositories: HashMap<RepoId, Entity<Repo>>,
    active_repo: Option<Entity<Repo>>,
    _subscriptions: Vec<Subscription>,
}

pub enum VcsStoreEvent {
    StatusesChanged(RepoId),
    ActiveRepoChanged,
    CommitCompleted,
}

impl EventEmitter<VcsStoreEvent> for VcsStore {}

impl VcsStore {
    pub fn new(cx: &mut Context<Self>) -> Self {
        // Set up any worktree/fs subscriptions here.
        Self { repositories: HashMap::new(), active_repo: None, _subscriptions: vec![] }
    }

    pub fn add_repo(&mut self, backend: Arc<dyn Repository>, cx: &mut Context<Self>) {
        let id = next_id();
        let repo = cx.new(|cx| Repo::new(backend, cx));

        let sub = cx.observe(&repo, |this, _, cx| {
            cx.emit(VcsStoreEvent::StatusesChanged(id));
        });
        self._subscriptions.push(sub);

        if self.active_repo.is_none() {
            self.active_repo = Some(repo.clone());
            cx.emit(VcsStoreEvent::ActiveRepoChanged);
        }
        self.repositories.insert(id, repo);
    }
}
```

### 5.4  `Repo` entity skeleton with job queue

```rust
pub struct Repo {
    statuses: SumTree<StatusEntry>,
    pending_ops: HashMap<RepoPath, PendingOp>,
    backend: Arc<dyn Repository>,
    job_tx: mpsc::UnboundedSender<RepoJob>,
    _worker: Task<()>,  // keeps the worker alive
}

enum RepoJob {
    RefreshStatus,
    Stage(Vec<RepoPath>),
    Unstage(Vec<RepoPath>),
}

impl Repo {
    pub fn new(backend: Arc<dyn Repository>, cx: &mut Context<Self>) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<RepoJob>();
        let b = backend.clone();
        let entity = cx.entity().downgrade();

        let worker = cx.background_spawn(async move {
            while let Some(job) = rx.next().await {
                match job {
                    RepoJob::RefreshStatus => {
                        let entries = b.status().unwrap_or_default();
                        entity.update(&mut cx.clone(), |repo, cx| {
                            repo.statuses = SumTree::from_iter(entries, &());
                            cx.notify();
                        }).ok();
                    }
                    RepoJob::Stage(paths) => {
                        b.stage(&paths, &HashMap::new()).ok();
                        // refresh after
                        let entries = b.status().unwrap_or_default();
                        entity.update(&mut cx.clone(), |repo, cx| {
                            repo.statuses = SumTree::from_iter(entries, &());
                            for p in &paths { repo.pending_ops.remove(p); }
                            cx.notify();
                        }).ok();
                    }
                    // ...
                }
            }
        });

        Self {
            statuses: SumTree::default(),
            pending_ops: HashMap::new(),
            backend,
            job_tx: tx,
            _worker: worker,
        }
    }

    pub fn stage(&mut self, paths: Vec<RepoPath>, cx: &mut Context<Self>) {
        for p in &paths { self.pending_ops.insert(p.clone(), PendingOp::Staging); }
        cx.notify();  // optimistic UI
        self.job_tx.send(RepoJob::Stage(paths)).ok();
    }
}
```

### 5.5  `BufferDiff` minimal implementation

If your project doesn't need the full gutter/staging system, you can
implement a simplified version:

```rust
pub struct SimpleDiff {
    buffer: Entity<Buffer>,
    base_text: Option<String>,
    pub hunks: Vec<SimpleHunk>,
    _sub: Subscription,
}

pub struct SimpleHunk {
    pub buffer_rows: Range<u32>,
    pub base_rows: Range<u32>,
    pub kind: HunkKind,
}
pub enum HunkKind { Added, Modified, Deleted }

impl SimpleDiff {
    pub fn new(buffer: Entity<Buffer>, base: Option<String>, cx: &mut Context<Self>) -> Self {
        let sub = cx.observe(&buffer, |this, buf, cx| {
            this.recompute(buf.read(cx).snapshot(), cx);
        });
        let mut this = Self { buffer: buffer.clone(), base_text: base, hunks: vec![], _sub: sub };
        this.recompute(buffer.read(cx).snapshot(), cx);
        this
    }

    fn recompute(&mut self, snap: BufferSnapshot, cx: &mut Context<Self>) {
        let base = self.base_text.as_deref().unwrap_or("");
        let current = snap.text();
        self.hunks = compute_line_diff(base, &current);
        cx.notify();
    }
}

fn compute_line_diff(base: &str, current: &str) -> Vec<SimpleHunk> {
    // Use the `imara-diff` crate:
    // imara_diff::diff(Algorithm::Myers, &base_lines, &current_lines, |change| { ... })
    todo!()
}
```

### 5.6  Panel subscription wiring (complete example)

```rust
pub struct VcsPanel {
    git_store: Entity<VcsStore>,
    entries: Vec<PanelEntry>,
    selected: Option<usize>,
    focus_handle: FocusHandle,
    scroll_handle: UniformListScrollHandle,
    _subs: Vec<Subscription>,
}

impl VcsPanel {
    pub fn new(git_store: Entity<VcsStore>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        // Subscribe to store-level events for entry rebuilding:
        let store_sub = cx.subscribe(&git_store, |this, _store, event, cx| {
            match event {
                VcsStoreEvent::StatusesChanged(_) => this.rebuild_entries(cx),
                VcsStoreEvent::ActiveRepoChanged  => this.rebuild_entries(cx),
                VcsStoreEvent::CommitCompleted    => cx.notify(),
            }
        });

        // Also observe the active repo directly for live updates:
        let repo_obs = if let Some(repo) = git_store.read(cx).active_repo.clone() {
            Some(cx.observe(&repo, |this, _, cx| {
                this.rebuild_entries(cx);
            }))
        } else { None };

        let mut this = Self {
            git_store,
            entries: vec![],
            selected: None,
            focus_handle: cx.focus_handle(),
            scroll_handle: UniformListScrollHandle::new(),
            _subs: vec![store_sub]
                .into_iter()
                .chain(repo_obs)
                .collect(),
        };
        this.rebuild_entries(cx);
        this
    }

    fn rebuild_entries(&mut self, cx: &mut Context<Self>) {
        let store = self.git_store.read(cx);
        self.entries = if let Some(repo) = &store.active_repo {
            repo.read(cx).statuses
                .iter()
                .map(|e| PanelEntry::from_status(e))
                .collect()
        } else {
            vec![]
        };
        cx.notify();
    }
}

impl Render for VcsPanel {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .key_context("VcsPanel")
            .track_focus(&self.focus_handle)
            .size_full()
            .child(
                uniform_list(
                    cx.entity().clone(),
                    "vcs-entries",
                    self.entries.len(),
                    |this, range, window, cx| {
                        range.map(|i| this.render_entry(i, window, cx)).collect()
                    },
                )
                .track_scroll(self.scroll_handle.clone())
                .flex_grow()
            )
    }
}
```

### 5.7  Critical pitfalls when replicating this pattern

| Pitfall | Symptom | Fix |
|---------|---------|-----|
| Dropping `_subscriptions` | Panel never updates after first render | Store all `Subscription` returns in `Vec<Subscription>` field |
| Strong reference to `Workspace` | Memory leak / cycle | Use `WeakEntity<Workspace>`, upgrade in closures |
| Blocking UI thread with git I/O | Frozen window during `git status` | All git operations via `cx.background_spawn()` job queue |
| Forgetting `cx.notify()` after job | State changes but no repaint | Always call `cx.notify()` after mutating entity state |
| Using `Entity::read` inside `update` | Borrow conflict panic | Use the `&mut Self` reference already provided by `update()` |
| Dropping the background worker `Task` | Job channel closed immediately | Store worker `Task<()>` in a field, never in a local variable |
| Variable row heights in `uniform_list` | Incorrect scroll positions / invisible entries | Every row must render at the exact same `px(height)` |
| Tree-view index confusion | Wrong entry opened on click | Resolve visible index through `TreeViewState::logical_indices` before accessing `entries[i]` |
| Two staged diffs out of sync | Wrong `SecondaryHunkStatus` shown | `BufferDiff::secondary_diff` must be updated whenever the index changes (not just HEAD changes) |
| Not refreshing base text after staging | Stale diff display | After any `StageEntries` / `SetIndexText` job, re-read `index_text_for_path` and call `BufferDiff::update_base_text()` |

---

## Part 6 — Complete Data-Flow Diagram

```
User clicks "Stage file.rs"
        │
        ▼
GitPanel::on_stage(entry, cx)
        │  cx.listener()
        ▼
repo.update(cx, |r, cx| r.stage_entries([path], cx))
        │
        ├─ pending_ops.insert(path, Staging)
        ├─ cx.notify()  ──────────────────────────► Frame N: renders with optimistic checkbox
        └─ job_tx.send(StageEntries { paths, tx })
                │
                ▼  (background thread)
        GitJob::StageEntries handler
                │
                ├─ git_repo.stage_paths(&paths, &env)
                │       └─ git2: write index via blob OID
                │
                ├─ git_repo.status()
                │       └─ git2: read all statuses
                │
                └─ entity.update(cx, |repo, cx|
                        ├─ repo.statuses = SumTree::from_iter(entries)
                        ├─ pending_ops.remove(path)
                        ├─ cx.notify()
                        └─ tx.send(Ok(()))
                                │
                ◄───────────────┘
        Frame N+1: renders with real checkbox from new SumTree
                │
                └─ cx.emit(VcsStoreEvent::StatusesChanged)
                        │
                        ▼
                GitStore subscription fires
                        │
                        └─ refresh_unstaged_diffs_for_repo(id, cx)
                                │
                                ├─ git_repo.index_text_for_path(path) → new base
                                └─ unstaged_diff.update(cx, |d, cx|
                                        ├─ d.snapshot.base_text = new_base
                                        └─ d.recalculate(buffer.snapshot(), cx)
                                                │
                                                ▼  (background)
                                        imara_diff::diff(new_base, current_text)
                                                │
                                        d.snapshot.hunks = new_hunks
                                        cx.notify()
                                                │
                                        ◄───────┘
                        Frame N+2: editor gutter updated with new diff state
```

---

## Summary Reference Table

| Concept | Zed type | Your implementation |
|---|---|---|
| Pure git backend | `Arc<dyn GitRepository>` | Your own trait + impl |
| Status ground truth | `SumTree<StatusEntry>` | `Vec<StatusEntry>` (or SumTree if sorted queries needed) |
| Per-repo reactive state | `Entity<Repository>` | `Entity<YourRepo>` |
| All-repo coordinator | `Entity<GitStore>` | `Entity<YourVcsStore>` |
| Async I/O isolation | `mpsc::UnboundedSender<GitJob>` + `cx.background_spawn()` worker | Same pattern |
| Optimistic UI | `pending_ops: HashMap<RepoPath, PendingOp>` | Same |
| Buffer diff | `Entity<BufferDiff>` + `BufferDiffSnapshot` | Can use `imara-diff` directly |
| Staging status | `SecondaryHunkStatus` from diff-of-diffs | Compute by comparing two diff results |
| Remote operations | `GitStoreMode::Remote { upstream }` + protobuf RPC | Omit unless needed |
| Credential prompts | AskPass helper binary + Unix socket | Omit unless needed |
| Test double | `FakeGitRepository` with `FakeFs` | Implement your trait with an in-memory backend |