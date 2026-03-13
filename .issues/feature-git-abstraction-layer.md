# Git Abstraction Layer

| Field        | Value       |
| ------------ | ----------- |
| **Type**     | `feature`   |
| **Priority** | `p1`        |
| **Parent**   | —           |
| **Created**  | 2026-03-13  |

## Summary

Build a three-layer git abstraction for cupel modeled after Zed's git crates: a pure git2/CLI wrapper (`src/git/`), GPUI entity wrappers (`GitStore`, `Repository`), and per-buffer diff state (`BufferDiff`). This is the foundational infrastructure that all git UI features depend on.

## User Story

As a cupel developer, I want a layered git abstraction so that git operations are testable, async-safe, and decoupled from the UI layer.

## Dependency Graph

```
#1 (scaffolding)
├── #2 (data types)  ─┐
├── #3 (error types) ─┤
│                      ├── #4 (trait) ──┬── #5 (real readonly) ── #6 (real mutating) ──┐
│                      │                └── #7 (fake repo) ────────────────────────────┤
│                      │                                                                ├── #8 (repo entity) ── #9 (git store) ── #10 (events/optimistic)
└── #11 (buffer diff, only needs imara-diff dep) ── #12 (secondary hunk status, also needs #10)
```

## Success Criteria

- [ ] `src/git/` module compiles with all types, traits, and implementations.
- [ ] `GitRepository` trait has ~40 methods covering status, branch, blame, staging, commit, push/pull, stash, log, and conflict resolution.
- [ ] `RealGitRepository` wraps git2 for local ops and shells out to `git` CLI for network ops.
- [ ] `FakeGitRepository` enables fully in-memory testing without a real repo.
- [ ] `Repository` GPUI entity serializes git ops through an async job queue.
- [ ] `GitStore` GPUI entity acts as a repository registry with event emission.
- [ ] `BufferDiff` entity keeps diff hunks up-to-date as buffers change.
- [ ] `SecondaryHunkStatus` enables staging-aware diff display.
- [ ] All layers compile and pass tests independently.

## Design Considerations

- Follow Zed's layered architecture: `git` (pure) → `project` (entities) → `git_ui` (rendering).
- The pure layer has no GPUI, no async runtime — just synchronous trait methods.
- `git2::Repository` is not `Sync`; wrap in `Mutex` inside `RealGitRepository`.
- Network operations (push/pull/fetch) use `git` CLI subprocess, not git2.
- Use `imara-diff` for the diff algorithm (replaces `similar`).
- The `env: &HashMap<String, String>` parameter pattern on mutating methods supports future credential helper injection.

## Out of Scope

- Git UI panel (GitPanel, ProjectDiff, ConflictView) — that's a separate epic.
- AskPass credential helper binary — can use empty env for local-only operations initially.
- Remote mode (`GitStoreMode::Remote`) and protobuf RPC — local-only for Phase 0.
- Worktree scanner integration — repositories will be added manually for now.

## Reference

- Architecture reference: [.epic/git.md](../.epic/git.md)

---

## Layer 1 — Pure Git Abstraction

### Module Scaffolding

| Field        | Value       |
| ------------ | ----------- |
| **Type**     | `feature`   |
| **Priority** | `p1`        |
| **Created**  | 2026-03-13  |

**Summary:** Create the `src/git/` module layout with `mod.rs` and empty submodule files, and add the `git2` and `imara-diff` dependencies to `Cargo.toml`. This is the foundation that all other git issues build on.

**Detailed Description:**

Replace the current `src/git.rs` stub with a `src/git/` directory containing:

```
src/git/
├── mod.rs           # re-exports
├── repository.rs    # GitRepository trait (issue #4)
├── real_repo.rs     # RealGitRepository (issues #5, #6)
├── fake_repo.rs     # FakeGitRepository (issue #7)
├── types.rs         # data types (issue #2)
├── status.rs        # StatusEntry + related types
├── error.rs         # GitError (issue #3)
└── diff.rs          # BufferDiff types (issue #11)
```

Add to `Cargo.toml` dependencies:
- `git2` — libgit2 bindings
- `imara-diff` — diff algorithm

Wire `mod git` into `src/lib.rs`.

**Success Criteria:**

- [ ] `src/git.rs` is replaced by `src/git/mod.rs` with submodule declarations.
- [ ] All submodule files exist (can be empty or contain minimal placeholder structs).
- [ ] `git2` and `imara-diff` are added to `[dependencies]` in `Cargo.toml`.
- [ ] `cargo check` passes.
- [ ] `src/lib.rs` declares `pub mod git`.

**Design Considerations:**

- Keep `mod.rs` re-exports minimal for now; each issue will add its own public items.
- Use the same module naming as Zed's `crates/git/src/` for familiarity.
- The `diff.rs` module is for buffer-diff types used in Layer 3; it only needs the `imara-diff` dep from this issue.

**Test Guidance:**

- Primary category: Integration tests
- `cargo check` passes with the new module structure.
- All modules are importable from `cupel::git::*`.
- Ensure no circular dependencies between submodules.

### Data Types

| Field        | Value       |
| ------------ | ----------- |
| **Type**     | `feature`   |
| **Priority** | `p1`        |
| **Created**  | 2026-03-13  |

**Summary:** Define the core git data types in `src/git/types.rs` and `src/git/status.rs`: `RepoPath`, `StatusEntry`, `GitFileStatus`, `FileStatus`, `Branch`, `UpstreamBranch`, `BlameEntry`, `CommitOptions`, `CommitSummary`, `CommitDetails`, `StashEntry`, `ConflictSide`, and `PushOptions`. These are the value types used throughout the git abstraction.

**Detailed Description:**

#### `src/git/types.rs`

```rust
pub struct RepoPath(Arc<Path>);                // repo-relative path, implements Ord
pub struct Branch { name, upstream, is_head, unix_timestamp }
pub struct UpstreamBranch { name, ahead, behind }
pub struct BlameEntry { sha, line_range, author, author_mail, author_time, committer, summary }
pub struct CommitOptions { amend, signoff }
pub struct PushOptions { force, set_upstream }
pub struct CommitSummary { sha, message, author, timestamp }
pub struct CommitDetails { summary, diff_stats, parent_shas }
pub struct StashEntry { index, message, sha }
pub enum ConflictSide { Ours, Theirs, Base }
```

#### `src/git/status.rs`

```rust
pub struct StatusEntry { repo_path, status }
pub struct GitFileStatus { index_status, worktree_status, conflict }
pub enum FileStatus { Added, Modified, Deleted, Untracked, Unchanged }
```

`RepoPath` should implement `Clone`, `Debug`, `PartialEq`, `Eq`, `PartialOrd`, `Ord`, `Hash`, and `Display`. It wraps `Arc<Path>` for cheap cloning.

`StatusEntry` staging state is derived from comparing `index_status` and `worktree_status` — provide a helper method `staging_state()` or similar.

**Success Criteria:**

- [ ] All listed types are defined with appropriate derives.
- [ ] `RepoPath` implements `Ord`, `Hash`, `Display`, and `From<&str>` / `From<PathBuf>`.
- [ ] `StatusEntry` has a method to derive staging state (fully staged, fully unstaged, partially staged).
- [ ] `FileStatus` enum covers Added, Modified, Deleted, Untracked, Unchanged.
- [ ] `cargo check` passes.

**Design Considerations:**

- Use `Arc<Path>` inside `RepoPath` for cheap cloning — these are passed around frequently.
- Keep types `Send + Sync` since they cross thread boundaries via the job queue.
- Don't add SumTree integration in this issue — that's an optimization for later.
- `BlameEntry` uses `String` for SHA rather than a git2-specific OID type to keep the pure layer backend-agnostic.

**Test Guidance:**

- Primary category: Unit tests
- `RepoPath` ordering matches lexicographic path ordering.
- `RepoPath` round-trips through `Display` / `From<&str>`.
- `StatusEntry::staging_state()` returns correct state for all combinations of `index_status` and `worktree_status`.
- Boundary: `RepoPath` with empty path, paths with special characters, nested paths.
- Boundary: `GitFileStatus` where both `index_status` and `worktree_status` are `Unchanged`.

### Error Types

| Field        | Value       |
| ------------ | ----------- |
| **Type**     | `feature`   |
| **Priority** | `p1`        |
| **Created**  | 2026-03-13  |

**Summary:** Define the `GitError` enum in `src/git/error.rs` using `thiserror`. This provides structured, descriptive error types for all git operations, wrapping `git2::Error`, `std::io::Error`, and CLI process failures.

**Detailed Description:**

```rust
#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("git2 error: {0}")]
    Git2(#[from] git2::Error),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("git CLI failed (exit {exit_code}): {stderr}")]
    CliError { exit_code: i32, stderr: String },

    #[error("repository not found at {path}")]
    RepoNotFound { path: PathBuf },

    #[error("path not found in repository: {0}")]
    PathNotFound(RepoPath),

    #[error("invalid operation: {0}")]
    InvalidOperation(String),

    #[error("merge conflict in {path}")]
    MergeConflict { path: RepoPath },
}

pub type GitResult<T> = Result<T, GitError>;
```

**Success Criteria:**

- [ ] `GitError` enum is defined with `thiserror` derives.
- [ ] `From<git2::Error>` and `From<std::io::Error>` conversions exist.
- [ ] `GitResult<T>` type alias is defined.
- [ ] Error messages include enough context for debugging (paths, exit codes, stderr).
- [ ] `cargo check` passes.

**Design Considerations:**

- Use `thiserror` which is already in `Cargo.toml`.
- Keep variants focused on categories of failure, not one variant per method.
- `CliError` captures stderr from git subprocess failures — essential for debugging push/pull issues.
- Consider `#[error(transparent)]` for wrapped errors if the inner message is sufficient.

**Test Guidance:**

- Primary category: Unit tests
- `GitError::from(git2::Error)` produces a `Git2` variant.
- `GitError::from(io::Error)` produces an `Io` variant.
- Error `Display` output includes relevant context.
- Boundary: `CliError` with empty stderr.
- Boundary: `CliError` with multi-line stderr.

### Repository Trait

| Field        | Value       |
| ------------ | ----------- |
| **Type**     | `feature`   |
| **Priority** | `p1`        |
| **Created**  | 2026-03-13  |

**Summary:** Define the `GitRepository` trait in `src/git/repository.rs` with ~40 methods covering identity, staging, commit, remote ops, status, branches, diff/content, blame, stash, history, and conflict resolution. This is the central abstraction that both `RealGitRepository` and `FakeGitRepository` implement.

**Detailed Description:**

The trait should be `Send + Sync` and use only types from `src/git/types.rs` and `src/git/error.rs`. It must have no GPUI dependencies.

#### Method groups (~40 methods):

**Identity (2):**
- `fn path(&self) -> &Path` — .git directory path
- `fn work_directory(&self) -> Option<&Path>`

**Staging/Index (4):**
- `fn stage_paths(&self, paths: &[RepoPath], env: &HashMap<String, String>) -> GitResult<()>`
- `fn unstage_paths(&self, paths: &[RepoPath], env: &HashMap<String, String>) -> GitResult<()>`
- `fn set_index_text(&self, path: &RepoPath, content: Option<String>, env: &HashMap<String, String>) -> GitResult<()>`
- `fn reload_index(&self)`

**Commit (2):**
- `fn commit(&self, message: &str, options: &CommitOptions, env: &HashMap<String, String>) -> GitResult<()>`
- `fn uncommit(&self, env: &HashMap<String, String>) -> GitResult<()>`

**Remote (4):**
- `fn push(&self, branch: &str, remote: Option<&str>, options: &PushOptions, env: &HashMap<String, String>) -> GitResult<()>`
- `fn pull(&self, rebase: bool, env: &HashMap<String, String>) -> GitResult<()>`
- `fn fetch(&self, env: &HashMap<String, String>) -> GitResult<()>`
- `fn create_remote(&self, name: &str, url: &str) -> GitResult<()>`

**Status (2):**
- `fn status(&self, path_prefixes: &[RepoPath]) -> GitResult<Vec<StatusEntry>>`
- `fn status_for_path(&self, path: &RepoPath) -> GitResult<Option<StatusEntry>>`

**Branch (6):**
- `fn current_branch(&self) -> Option<Branch>`
- `fn branches(&self) -> GitResult<Vec<Branch>>`
- `fn create_branch(&self, name: &str) -> GitResult<()>`
- `fn checkout(&self, target: &str, env: &HashMap<String, String>) -> GitResult<()>`
- `fn delete_branch(&self, name: &str) -> GitResult<()>`
- `fn merge_base(&self, a: &str, b: &str) -> GitResult<Option<String>>`
- `fn remote_url(&self, name: &str) -> Option<String>`

**Diff/Content (2):**
- `fn head_text_for_path(&self, path: &RepoPath) -> GitResult<Option<String>>`
- `fn index_text_for_path(&self, path: &RepoPath) -> GitResult<Option<String>>`

**Blame (1):**
- `fn blame_for_path(&self, path: &RepoPath, content: &str) -> GitResult<Vec<BlameEntry>>`

**Stash (5):**
- `fn stash_list(&self) -> GitResult<Vec<StashEntry>>`
- `fn stash_all(&self, message: Option<&str>) -> GitResult<()>`
- `fn stash_pop(&self, index: usize) -> GitResult<()>`
- `fn stash_apply(&self, index: usize) -> GitResult<()>`
- `fn stash_drop(&self, index: usize) -> GitResult<()>`

**History (2):**
- `fn log(&self, path: Option<&RepoPath>, limit: usize) -> GitResult<Vec<CommitSummary>>`
- `fn show(&self, oid: &str) -> GitResult<CommitDetails>`

**Conflict (1):**
- `fn checkout_conflict_path(&self, path: &RepoPath, side: ConflictSide) -> GitResult<()>`

**Success Criteria:**

- [ ] `GitRepository` trait is defined with `Send + Sync` bounds.
- [ ] All ~40 methods are declared with correct signatures using project types.
- [ ] No GPUI or async types in the trait — purely synchronous.
- [ ] The `env` parameter is present on all mutating operations.
- [ ] `cargo check` passes.

**Design Considerations:**

- The trait is intentionally synchronous. Async wrapping happens in the Repository entity (issue #8).
- `env` parameter on mutating methods supports credential helper injection via `GIT_ASKPASS`.
- Use `&str` for content parameters passed to blame rather than `Rope` to keep the pure layer free of editor types.
- Consider using `&[RepoPath]` rather than owned `Vec` for input parameters.

**Test Guidance:**

- Primary category: Integration tests
- Trait is object-safe: `Arc<dyn GitRepository>` compiles.
- All method signatures use only types from this crate.
- Ensure `dyn GitRepository` can be used as `Send + Sync` trait object.

### Real Repository — Read-Only Methods

| Field        | Value       |
| ------------ | ----------- |
| **Type**     | `feature`   |
| **Priority** | `p1`        |
| **Created**  | 2026-03-13  |

**Summary:** Implement `RealGitRepository` struct and its read-only `GitRepository` trait methods using git2: identity, status, branch queries, blame, log/show, stash list, head/index text retrieval, and merge base. Mutating methods are stubbed with `todo!()` for issue #6.

**Detailed Description:**

#### Struct definition

```rust
pub struct RealGitRepository {
    repository: Mutex<git2::Repository>,
    path: PathBuf,
    work_directory: Option<PathBuf>,
}
```

`git2::Repository` is not `Sync`, so it must be wrapped in `Mutex`. The `path` and `work_directory` are cached at construction time to avoid locking for identity queries.

#### Constructor

```rust
impl RealGitRepository {
    pub fn open(path: &Path) -> GitResult<Self> {
        let repo = git2::Repository::open(path)?;
        // cache work_directory and path
    }
}
```

#### Read-only methods to implement

- **`path()`** / **`work_directory()`** — return cached values, no lock needed.
- **`status()`** — use `repo.statuses()` with `StatusOptions::include_untracked(true)`, map `git2::Status` bitmask to `GitFileStatus`.
- **`status_for_path()`** — filter from status or use `repo.status_file()`.
- **`current_branch()`** — `repo.head()` → `reference.shorthand()`.
- **`branches()`** — `repo.branches(None)` → map to `Branch` structs.
- **`merge_base()`** — `repo.merge_base(oid_a, oid_b)`.
- **`remote_url()`** — `repo.find_remote(name)` → `.url()`.
- **`head_text_for_path()`** — `repo.head()` → tree → blob → content as UTF-8.
- **`index_text_for_path()`** — `repo.index()` → `get_path()` → `repo.find_blob()` → content.
- **`blame_for_path()`** — `repo.blame_file()` → map hunks to `BlameEntry`.
- **`stash_list()`** — `repo.stash_foreach()` → collect into `Vec<StashEntry>`.
- **`log()`** — `repo.revwalk()` → iterate and collect `CommitSummary`.
- **`show()`** — `repo.find_commit()` → `CommitDetails` with diff stats.
- **`reload_index()`** — `repo.index()?.read(true)`.

#### Stubbed methods

All mutating methods (`stage_paths`, `unstage_paths`, `set_index_text`, `commit`, `uncommit`, `push`, `pull`, `fetch`, `create_remote`, `create_branch`, `checkout`, `delete_branch`, `stash_all/pop/apply/drop`, `checkout_conflict_path`) should be `todo!("implemented in issue #6")`.

**Success Criteria:**

- [ ] `RealGitRepository` struct is defined with `Mutex<git2::Repository>`.
- [ ] All read-only trait methods are implemented using git2.
- [ ] Mutating methods are stubbed with `todo!()`.
- [ ] Status mapping correctly translates git2 bitmask to `GitFileStatus`.
- [ ] `cargo check` passes.
- [ ] Tests pass against a real temporary git repo.

**Design Considerations:**

- Lock the `Mutex` only for the duration of each method call — don't hold it across methods.
- `git2::Repository::statuses()` can be slow on large repos; this is fine since it runs on a background thread via the job queue.
- Use `repo.revwalk()` with `set_sorting(Sort::TIME)` for log.
- Return `Ok(None)` rather than errors for missing paths in `head_text_for_path` / `index_text_for_path`.
- For blame, pass `content` as `&str` not `Rope` — the trait is editor-type-free.

**Test Guidance:**

- Primary category: Integration tests
- Open a real temporary git repo (use `tempfile` + `git2::Repository::init`).
- `status()` on a repo with added, modified, deleted, and untracked files.
- `current_branch()` returns `"main"` (or default branch) on a fresh repo.
- `head_text_for_path()` returns committed file content.
- `index_text_for_path()` returns staged file content.
- `log()` returns commit history in reverse chronological order.
- `blame_for_path()` returns correct line attributions.
- Boundary: Empty repository (no commits): status, branch, and log should handle gracefully.
- Boundary: Binary files: `head_text_for_path` should handle or error clearly.
- Infrastructure: `tempfile` crate for temporary directories, `git2::Repository::init()` for test repos.

### Real Repository — Mutating Methods

| Field        | Value       |
| ------------ | ----------- |
| **Type**     | `feature`   |
| **Priority** | `p1`        |
| **Created**  | 2026-03-13  |

**Summary:** Implement the mutating `GitRepository` trait methods on `RealGitRepository`: staging/unstaging, index writes, commit/uncommit, push/pull/fetch, branch creation/deletion/checkout, stash operations, remote creation, and conflict resolution. Local index operations use git2; network and complex operations use the git CLI.

**Detailed Description:**

#### git2-based methods (local index operations)

- **`stage_paths()`** — shell out to `git add` via CLI (git2's staging API is limited for directory patterns). Alternatively, use `repo.index()` → `add_path()` → `write()`.
- **`unstage_paths()`** — `git reset HEAD -- <paths>` via CLI, or use git2 to restore index entries from HEAD tree.
- **`set_index_text()`** — `repo.blob(content)` → `index.add()` with constructed `IndexEntry` → `index.write()`. For `None` content: `index.remove_path()` → `index.write()`.
- **`commit()`** — use git2: `repo.index()` → `write_tree()` → `repo.commit()` with HEAD as parent. Handle `CommitOptions::amend` by using `repo.head().target()` as the commit to amend.
- **`uncommit()`** — equivalent to `git reset HEAD^ --soft`: move HEAD to parent commit.
- **`create_branch()`** — `repo.branch(name, &head_commit, false)`.
- **`delete_branch()`** — find branch → `branch.delete()`.
- **`checkout()`** — CLI: `git checkout <target>` (git2's checkout is complex and error-prone).

#### CLI-based methods (network operations)

These shell out to `git` with the provided `env` map:

- **`push()`** — `git push [remote] [branch] [--force] [--set-upstream]`
- **`pull()`** — `git pull [--rebase]`
- **`fetch()`** — `git fetch`
- **`create_remote()`** — `git remote add <name> <url>`

#### Stash methods (git2 or CLI)

- **`stash_all()`** — `git stash push -m <message>` or git2 `repo.stash_save()`.
- **`stash_pop()`** — `git stash pop stash@{index}`.
- **`stash_apply()`** — `git stash apply stash@{index}`.
- **`stash_drop()`** — `git stash drop stash@{index}`.

#### Conflict resolution

- **`checkout_conflict_path()`** — `git checkout --ours/--theirs <path>`.

#### CLI helper

```rust
fn run_git(&self, args: &[&str], env: &HashMap<String, String>) -> GitResult<String> {
    let output = Command::new("git")
        .args(args)
        .envs(env)
        .current_dir(self.work_directory.as_ref().unwrap_or(&self.path))
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
```

**Success Criteria:**

- [ ] All mutating trait methods are implemented (replacing `todo!()` stubs from issue #5).
- [ ] Index operations use git2 for speed.
- [ ] Network operations use git CLI for credential helper compatibility.
- [ ] `set_index_text()` correctly writes blob → index entry → index.write().
- [ ] `commit()` handles both normal and amend cases.
- [ ] CLI helper captures stderr into `GitError::CliError`.
- [ ] `cargo check` passes.

**Design Considerations:**

- Prefer git CLI for operations where git2's API is cumbersome or limited (checkout, stash).
- The `env` parameter must be forwarded to all CLI calls — it will contain `GIT_ASKPASS` in the future.
- `set_index_text()` is the mechanism for hunk-level staging: the caller computes desired index content and writes it directly.
- After any index mutation, callers should call `reload_index()`.

**Test Guidance:**

- Primary category: Integration tests
- `stage_paths()` moves a file from unstaged to staged (verify with subsequent `status()` call).
- `unstage_paths()` moves a file from staged to unstaged.
- `set_index_text()` writes specific content to the index (verify with `index_text_for_path()`).
- `commit()` creates a new commit (verify with `log()`).
- `commit()` with amend replaces the last commit.
- `uncommit()` moves HEAD back one commit.
- `create_branch()` and `delete_branch()` roundtrip.
- CLI helper returns `CliError` on bad commands.
- Boundary: Staging a file that's already staged.
- Boundary: Committing with an empty index.
- Boundary: Uncommit on a repo with only one commit (should error).
- Boundary: `set_index_text(path, None)` removes a path from the index.
- Infrastructure: Same test repo infrastructure from issue #5. Tests for CLI methods need `git` binary available in PATH.

### Fake Repository Test Double

| Field        | Value       |
| ------------ | ----------- |
| **Type**     | `feature`   |
| **Priority** | `p2`        |
| **Created**  | 2026-03-13  |

**Summary:** Implement `FakeGitRepository`, a fully in-memory `GitRepository` implementation for testing. It stores head, index, and working tree state in `HashMap`s and derives status by comparing them — no real git repo or filesystem needed.

**Detailed Description:**

```rust
pub struct FakeGitRepository {
    /// HEAD state: path → file content
    head: Mutex<HashMap<RepoPath, String>>,
    /// Index state: path → file content
    index: Mutex<HashMap<RepoPath, String>>,
    /// Working tree state: path → file content
    worktree: Mutex<HashMap<RepoPath, String>>,
    /// Current branch name
    current_branch: Mutex<Option<String>>,
    /// All branches
    branches: Mutex<Vec<Branch>>,
    /// Commit log
    commits: Mutex<Vec<CommitSummary>>,
    /// Identity
    path: PathBuf,
    work_directory: Option<PathBuf>,
}
```

#### Status derivation

Status is computed by comparing the three maps:

- File in worktree but not in head/index → `Untracked` / `Added`
- File in worktree differs from index → `worktree_status: Modified`
- File in index differs from head → `index_status: Modified` (staged)
- File in head but not in worktree → `Deleted`
- File in index but not in worktree → `worktree_status: Deleted`

#### Builder/setter API for test setup

```rust
impl FakeGitRepository {
    pub fn new(path: PathBuf) -> Self { ... }
    pub fn set_head_content(&self, path: RepoPath, content: &str) { ... }
    pub fn set_index_content(&self, path: RepoPath, content: &str) { ... }
    pub fn set_worktree_content(&self, path: RepoPath, content: &str) { ... }
    pub fn set_branch(&self, name: &str) { ... }
    pub fn add_commit(&self, summary: CommitSummary) { ... }
}
```

#### Trait implementation behavior

- **`stage_paths()`** — copy content from worktree to index map.
- **`unstage_paths()`** — copy content from head to index map (or remove if not in head).
- **`set_index_text()`** — directly set/remove index map entries.
- **`commit()`** — copy index to head, record a `CommitSummary`.
- **`head_text_for_path()`** — look up in head map.
- **`index_text_for_path()`** — look up in index map.
- **Network operations** (push/pull/fetch) — no-op or return `Ok(())`.

**Success Criteria:**

- [ ] `FakeGitRepository` implements all `GitRepository` trait methods.
- [ ] Status is correctly derived by comparing head, index, and worktree maps.
- [ ] Staging moves content from worktree to index.
- [ ] Committing moves content from index to head.
- [ ] Builder methods allow easy test setup.
- [ ] `cargo test` passes with fake repo tests.

**Design Considerations:**

- Use `Mutex` on all internal state to satisfy `Send + Sync` requirements.
- Network operations should be no-ops — the fake is for testing local workflows.
- The fake should be realistic enough that `GitStore` and `Repository` entity tests work without modification.
- Consider making `FakeGitRepository` `#[cfg(test)]` or gating behind a feature flag, though for now placing it in the main crate is fine since it has no heavy dependencies.

**Test Guidance:**

- Primary category: Unit tests
- Create a fake repo, set head and worktree content, verify `status()` returns correct entries.
- `stage_paths()` followed by `status()` shows files as staged.
- `commit()` followed by `status()` shows clean state.
- `set_index_text()` with specific content is reflected in `index_text_for_path()`.
- `head_text_for_path()` returns content set via `set_head_content()`.
- Boundary: Empty repository (all maps empty).
- Boundary: File only in worktree (untracked).
- Boundary: File only in head (deleted in worktree).
- Boundary: Same content in all three maps (unchanged).

---

## Layer 2 — GitStore & Repository Entities

### Repository Entity

| Field        | Value       |
| ------------ | ----------- |
| **Type**     | `feature`   |
| **Priority** | `p1`        |
| **Created**  | 2026-03-13  |

**Summary:** Implement the `Repository` GPUI entity that wraps `Arc<dyn GitRepository>` with an async job queue. All git operations are serialized through a background worker task, keeping the main thread free. The entity holds status state, a snapshot for cheap reads, and pending ops for optimistic UI.

**Detailed Description:**

#### Struct

```rust
pub struct Repository {
    // Identity
    pub dot_git_path: PathBuf,

    // Status
    pub statuses: Vec<StatusEntry>,
    pub snapshot: RepositorySnapshot,

    // Backend
    git_repo: Arc<dyn GitRepository>,

    // Job queue
    job_sender: mpsc::UnboundedSender<GitJob>,
    _worker: Task<()>,

    // Optimistic UI
    pending_ops: HashMap<RepoPath, PendingOp>,
}
```

#### `RepositorySnapshot`

```rust
#[derive(Clone, Default)]
pub struct RepositorySnapshot {
    pub branch: Option<Branch>,
    pub stash_count: usize,
}
```

Cheap-to-clone value type for UI reads without locking the entity.

#### `GitJob` enum

```rust
enum GitJob {
    RefreshStatus { path_prefixes: Vec<RepoPath> },
    Stage { paths: Vec<RepoPath> },
    Unstage { paths: Vec<RepoPath> },
    SetIndexText { path: RepoPath, content: Option<String> },
    Commit { message: String, options: CommitOptions },
    Push { branch: String, remote: Option<String>, options: PushOptions },
    Pull { rebase: bool },
    Fetch,
}
```

#### Background worker

The worker is spawned in `Repository::new()` and processes jobs sequentially:

1. Receive job from channel.
2. Execute the corresponding `GitRepository` method on the background thread.
3. After any mutating operation, automatically refresh status.
4. Call `entity.update(cx, |repo, cx| { repo.statuses = ...; cx.notify(); })` to push changes back to the main thread.

#### Public API

```rust
impl Repository {
    pub fn new(git_repo: Arc<dyn GitRepository>, cx: &mut Context<Self>) -> Self;
    pub fn stage(&mut self, paths: Vec<RepoPath>, cx: &mut Context<Self>);
    pub fn unstage(&mut self, paths: Vec<RepoPath>, cx: &mut Context<Self>);
    pub fn commit(&mut self, message: String, options: CommitOptions, cx: &mut Context<Self>);
    pub fn refresh_status(&self);
    pub fn snapshot(&self) -> &RepositorySnapshot;
    pub fn statuses(&self) -> &[StatusEntry];
    pub fn effective_status(&self, path: &RepoPath) -> Option<StagingState>;
}
```

`effective_status()` checks `pending_ops` first, falling back to `statuses`.

**Success Criteria:**

- [ ] `Repository` is a GPUI entity (`impl EventEmitter<RepositoryEvent> for Repository`).
- [ ] Background worker serializes all git operations.
- [ ] Status is automatically refreshed after mutating operations.
- [ ] `cx.notify()` is called when status changes, triggering UI re-renders.
- [ ] `RepositorySnapshot` provides cheap cloneable state for renders.
- [ ] `cargo check` passes.

**Design Considerations:**

- Use `mpsc::unbounded_channel` for the job queue — bounded channels risk deadlock if the worker is busy.
- The worker task must be stored in the struct (`_worker: Task<()>`) to keep it alive.
- Use `WeakEntity` in the worker closure to avoid preventing entity cleanup.
- `cx.notify()` is the mechanism that triggers GPUI re-renders when state changes.
- Consider emitting `RepositoryEvent::IndexChanged` after staging operations so GitStore can refresh buffer diffs.

**Test Guidance:**

- Primary category: Integration tests
- Create a `Repository` with `FakeGitRepository`, call `stage()`, verify status updates after job processes.
- `refresh_status()` correctly populates `statuses` from the backend.
- `commit()` creates a commit and refreshes status.
- Multiple jobs are processed in order.
- Boundary: Entity dropped while worker has pending jobs — should not panic.
- Boundary: Rapid successive `stage()` / `unstage()` calls.
- Infrastructure: GPUI test harness (`gpui::test` feature), `FakeGitRepository` from issue #7.

### GitStore Entity

| Field        | Value       |
| ------------ | ----------- |
| **Type**     | `feature`   |
| **Priority** | `p1`        |
| **Created**  | 2026-03-13  |

**Summary:** Implement the `GitStore` GPUI entity as a repository registry. It manages a `HashMap<RepositoryId, Entity<Repository>>`, tracks the active repository, emits `GitStoreEvent`s when state changes, and provides the public API that the UI panel will consume.

**Detailed Description:**

#### Struct

```rust
pub struct GitStore {
    repositories: HashMap<RepositoryId, Entity<Repository>>,
    active_repository: Option<Entity<Repository>>,
    next_repository_id: RepositoryId,
    _subscriptions: Vec<Subscription>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RepositoryId(u64);
```

#### Events

```rust
pub enum GitStoreEvent {
    ActiveRepositoryChanged,
    StatusesChanged(RepositoryId),
    CommitCompleted { repository_id: RepositoryId },
    BranchChanged,
}

impl EventEmitter<GitStoreEvent> for GitStore {}
```

#### Public API

```rust
impl GitStore {
    pub fn new(cx: &mut Context<Self>) -> Self;

    /// Add a repository to the store. Sets it as active if it's the first.
    pub fn add_repository(
        &mut self,
        git_repo: Arc<dyn GitRepository>,
        cx: &mut Context<Self>,
    ) -> RepositoryId;

    /// Remove a repository from the store.
    pub fn remove_repository(&mut self, id: RepositoryId, cx: &mut Context<Self>);

    /// Get the active repository entity.
    pub fn active_repository(&self) -> Option<&Entity<Repository>>;

    /// Set the active repository.
    pub fn set_active_repository(&mut self, id: RepositoryId, cx: &mut Context<Self>);

    /// Get a repository by ID.
    pub fn repository(&self, id: RepositoryId) -> Option<&Entity<Repository>>;

    /// Iterate all repositories.
    pub fn repositories(&self) -> impl Iterator<Item = (RepositoryId, &Entity<Repository>)>;
}
```

#### Observation pattern

When adding a repository, `GitStore` subscribes to it:

```rust
let sub = cx.observe(&repo_entity, move |this, _, cx| {
    cx.emit(GitStoreEvent::StatusesChanged(id));
});
self._subscriptions.push(sub);
```

This means any `cx.notify()` on a `Repository` entity automatically propagates as a `GitStoreEvent` to the panel.

**Success Criteria:**

- [ ] `GitStore` is a GPUI entity with `EventEmitter<GitStoreEvent>`.
- [ ] Repositories can be added, removed, and queried by ID.
- [ ] Active repository tracking works correctly.
- [ ] `GitStoreEvent::StatusesChanged` is emitted when any repository notifies.
- [ ] `GitStoreEvent::ActiveRepositoryChanged` is emitted on active repo change.
- [ ] `cargo check` passes.

**Design Considerations:**

- `RepositoryId` is a simple monotonic counter — not persisted across sessions.
- The store observes each `Repository` entity to re-emit events. This is the reactive chain: `git op → Repository.notify() → GitStore observes → GitStoreEvent → Panel re-renders`.
- For Phase 0, repositories are added manually via `add_repository()`. Automatic discovery from worktree scanning is future work.
- Keep the store simple — no buffer diff tracking here (that's future work or issue #11–#12 if integrated).

**Test Guidance:**

- Primary category: Integration tests
- Add a repository → it becomes active → `ActiveRepositoryChanged` event emitted.
- Add two repositories → first is active → set second active → event emitted.
- Repository notifies → `StatusesChanged` event emitted on GitStore.
- Remove active repository → active switches to another or `None`.
- Boundary: Remove the only repository.
- Boundary: Add repository with same path twice.
- Boundary: Query non-existent `RepositoryId`.
- Infrastructure: GPUI test harness, `FakeGitRepository` from issue #7.

### Events & Optimistic UI

| Field        | Value       |
| ------------ | ----------- |
| **Type**     | `feature`   |
| **Priority** | `p2`        |
| **Created**  | 2026-03-13  |

**Summary:** Add `pending_ops` optimistic UI support to the `Repository` entity and expand `GitStoreEvent` with `CommitCompleted` and `BranchChanged` variants. When the user clicks "Stage", the UI updates immediately without waiting for the background job to complete.

**Detailed Description:**

#### Optimistic state in Repository

```rust
pub enum PendingOp {
    Staging,
    Unstaging,
}
```

Before sending a job to the worker, `Repository` records pending state:

```rust
fn stage(&mut self, paths: Vec<RepoPath>, cx: &mut Context<Self>) {
    for path in &paths {
        self.pending_ops.insert(path.clone(), PendingOp::Staging);
    }
    cx.notify(); // immediate UI update with optimistic state

    self.job_sender.send(GitJob::Stage { paths: paths.clone() }).ok();

    // Spawn a task to clear pending_ops when the job completes
    let entity = cx.entity().downgrade();
    cx.spawn(|_window, mut cx| async move {
        // Wait for job completion signal
        // Then clear pending_ops and notify
    }).detach();
}
```

#### Effective status query

```rust
impl Repository {
    /// Returns the effective staging state, accounting for pending operations.
    pub fn effective_staging_state(&self, path: &RepoPath) -> StagingState {
        if let Some(pending) = self.pending_ops.get(path) {
            match pending {
                PendingOp::Staging => StagingState::Staged,
                PendingOp::Unstaging => StagingState::Unstaged,
            }
        } else {
            // Derive from actual status
            self.status_for(path)
                .map(|s| s.staging_state())
                .unwrap_or(StagingState::Unstaged)
        }
    }
}

pub enum StagingState {
    Staged,
    Unstaged,
    PartiallyStaged,
}
```

#### Additional GitStoreEvent variants

Expand events emitted by `GitStore` when it observes repository changes:

- `CommitCompleted { repository_id, was_amend }` — emitted after a successful commit job.
- `BranchChanged` — emitted after checkout or branch switch.

The Repository entity emits `RepositoryEvent` variants that GitStore translates into `GitStoreEvent`:

```rust
pub enum RepositoryEvent {
    StatusChanged,
    IndexChanged,
    CommitCompleted { was_amend: bool },
    BranchChanged,
}
```

#### Job completion signaling

Add `oneshot::Sender<GitResult<()>>` to mutating `GitJob` variants so the caller can be notified when the operation completes:

```rust
enum GitJob {
    Stage { paths: Vec<RepoPath>, tx: oneshot::Sender<GitResult<()>> },
    Commit { message: String, options: CommitOptions, tx: oneshot::Sender<GitResult<()>> },
    // ...
}
```

**Success Criteria:**

- [ ] `pending_ops` is populated before job submission and cleared after completion.
- [ ] `effective_staging_state()` returns optimistic state while ops are pending.
- [ ] `cx.notify()` fires immediately on pending_ops insertion (before job runs).
- [ ] `CommitCompleted` and `BranchChanged` events propagate through GitStore.
- [ ] `RepositoryEvent` enum is defined and emitted by Repository.
- [ ] `oneshot` channels provide job completion signaling.
- [ ] `cargo check` passes.

**Design Considerations:**

- Pending ops must be cleared even if the job fails — use `oneshot` receiver in a spawned task that always clears, regardless of result.
- If a job fails, the next status refresh will correct the UI to the actual state.
- Don't accumulate unbounded pending_ops — each `stage()` / `unstage()` call clears previous pending state for those paths.
- `StagingState` is a UI-facing enum, separate from `FileStatus`.

**Test Guidance:**

- Primary category: Integration tests
- Call `stage()` → `effective_staging_state()` immediately returns `Staged` (before job runs).
- After job completes, `pending_ops` is empty and actual status matches.
- Failed job still clears `pending_ops`.
- `CommitCompleted` event fires after successful commit.
- `BranchChanged` event fires after checkout.
- Boundary: Stage then immediately unstage the same path — latest op wins.
- Boundary: Multiple paths staged in one call.
- Boundary: Job completes after entity is dropped — should not panic.
- Infrastructure: GPUI test harness with async task execution, `FakeGitRepository` from issue #7.

---

## Layer 3 — Buffer Diff

### BufferDiff Entity

| Field        | Value       |
| ------------ | ----------- |
| **Type**     | `feature`   |
| **Priority** | `p2`        |
| **Created**  | 2026-03-13  |

**Summary:** Implement the `BufferDiff` GPUI entity that maintains an up-to-date set of diff hunks between a buffer's current content and a base text (from HEAD or index). Uses `imara-diff` for the diff algorithm and recalculates on every buffer change via GPUI observation.

**Detailed Description:**

#### Core types

```rust
pub struct BufferDiff {
    buffer: Entity<Buffer>,
    snapshot: BufferDiffSnapshot,
    diff_task: Option<Task<()>>,
    _subscription: Subscription,
}

#[derive(Clone, Default)]
pub struct BufferDiffSnapshot {
    pub hunks: Vec<DiffHunk>,
    pub base_text: Option<String>,
}

pub struct DiffHunk {
    /// Line range in the current buffer content.
    pub buffer_range: Range<u32>,
    /// Byte range in base_text that this hunk replaces.
    pub diff_base_byte_range: Range<usize>,
}
```

#### Construction

```rust
impl BufferDiff {
    pub fn new(
        buffer: Entity<Buffer>,
        base_text: Option<String>,
        cx: &mut Context<Self>,
    ) -> Self {
        let sub = cx.observe(&buffer, |this, _buffer, cx| {
            this.recalculate(cx);
        });
        let mut this = Self {
            buffer, snapshot: BufferDiffSnapshot::new(base_text),
            diff_task: None, _subscription: sub,
        };
        this.recalculate(cx);
        this
    }
}
```

#### Recalculation

`recalculate()` spawns a background task that:

1. Takes a snapshot of the current buffer text.
2. Runs `imara-diff` against `base_text`.
3. Converts imara-diff output into `Vec<DiffHunk>`.
4. Updates `self.snapshot.hunks` on the main thread and calls `cx.notify()`.

```rust
fn recalculate(&mut self, cx: &mut Context<Self>) {
    let buffer_text = self.buffer.read(cx).text();
    let base_text = self.snapshot.base_text.clone();

    self.diff_task = Some(cx.background_spawn(async move {
        compute_diff_hunks(&buffer_text, base_text.as_deref())
    }));
    // Await and update snapshot...
}
```

#### imara-diff integration

```rust
fn compute_diff_hunks(current: &str, base: Option<&str>) -> Vec<DiffHunk> {
    let base = match base {
        Some(b) => b,
        None => return vec![], // new file, no base to diff against
    };
    let input = imara_diff::intern::InternedInput::new(base, current);
    let diff = imara_diff::diff(
        imara_diff::Algorithm::Histogram,
        &input,
        imara_diff::sink::Counter::default(),
    );
    // Convert to DiffHunk using a custom Sink implementation
}
```

#### Snapshot API

```rust
impl BufferDiffSnapshot {
    pub fn hunks_intersecting_range(&self, range: Range<u32>) -> impl Iterator<Item = &DiffHunk>;
    pub fn is_empty(&self) -> bool;
    pub fn hunk_count(&self) -> usize;
}
```

#### Base text update

```rust
impl BufferDiff {
    /// Update the base text (e.g., when index changes after staging).
    pub fn set_base_text(&mut self, base_text: Option<String>, cx: &mut Context<Self>) {
        self.snapshot.base_text = base_text;
        self.recalculate(cx);
    }
}
```

**Success Criteria:**

- [ ] `BufferDiff` is a GPUI entity that observes a `Buffer` and recalculates on change.
- [ ] `imara-diff` is used for the diff algorithm.
- [ ] Diff recalculation happens on a background thread.
- [ ] `BufferDiffSnapshot` provides a cheap-to-clone value type with hunk data.
- [ ] `hunks_intersecting_range()` returns hunks overlapping a given line range.
- [ ] `set_base_text()` triggers recalculation.
- [ ] `cx.notify()` fires after each recalculation.
- [ ] `cargo check` passes.

**Design Considerations:**

- The `DiffHunk` in this issue uses `Range<u32>` for line ranges rather than `Range<Anchor>`. Anchor-based ranges require deeper buffer integration — start simple and upgrade later.
- Recalculation is debounced by replacing the previous `diff_task` — if the buffer changes rapidly, only the last calculation completes.
- `imara-diff::Algorithm::Histogram` is preferred over Myers for better results on code.
- `SecondaryHunkStatus` is NOT part of `DiffHunk` in this issue — that's issue #12.

**Test Guidance:**

- Primary category: Unit tests
- Diff of identical texts produces zero hunks.
- Diff of texts with added lines produces hunks with correct ranges.
- Diff of texts with deleted lines produces hunks with correct base byte ranges.
- Diff of texts with modified lines produces hunks.
- `hunks_intersecting_range()` returns correct subset.
- `set_base_text()` triggers re-diff with new hunks.
- Boundary: Empty base text (new file) → zero hunks.
- Boundary: Empty current text (deleted file) → one hunk covering entire base.
- Boundary: Very large texts (performance).
- Boundary: Base text is `None` → zero hunks.
- Infrastructure: GPUI test harness for entity construction and observation.

### Secondary Hunk Status

| Field        | Value       |
| ------------ | ----------- |
| **Type**     | `feature`   |
| **Priority** | `p2`        |
| **Created**  | 2026-03-13  |

**Summary:** Add `SecondaryHunkStatus` to `DiffHunk` and implement the diff-of-diffs algorithm. By comparing hunks from two `BufferDiff` instances (unstaged: worktree-vs-index, uncommitted: worktree-vs-HEAD), each hunk gains staging awareness — fully staged, fully unstaged, or partially staged.

**Detailed Description:**

#### SecondaryHunkStatus enum

```rust
pub enum SecondaryHunkStatus {
    /// Hunk does not exist in the secondary diff → fully staged.
    NoSecondaryHunk,
    /// Hunk exists identically in the secondary diff → fully unstaged.
    HasSecondaryHunk,
    /// Hunk partially overlaps the secondary diff → partially staged.
    OverlapsWithSecondaryHunk,
}
```

#### Add to DiffHunk

```rust
pub struct DiffHunk {
    pub buffer_range: Range<u32>,
    pub diff_base_byte_range: Range<usize>,
    pub secondary_status: SecondaryHunkStatus, // NEW
}
```

#### Secondary diff linking

`BufferDiff` gains a `secondary_diff` field:

```rust
pub struct BufferDiff {
    // ... existing fields
    secondary_diff: Option<WeakEntity<BufferDiff>>,
}

impl BufferDiff {
    pub fn set_secondary_diff(&mut self, other: WeakEntity<BufferDiff>, cx: &mut Context<Self>) {
        self.secondary_diff = Some(other);
        self.recalculate(cx);
    }
}
```

#### Diff-of-diffs algorithm

During `recalculate()`, after computing primary hunks, compare each hunk against the secondary diff's hunks:

```rust
fn compute_secondary_status(
    hunk: &DiffHunk,
    secondary_hunks: &[DiffHunk],
) -> SecondaryHunkStatus {
    let overlapping: Vec<_> = secondary_hunks
        .iter()
        .filter(|h| ranges_overlap(&h.buffer_range, &hunk.buffer_range))
        .collect();

    if overlapping.is_empty() {
        // Change is in index but not in HEAD → fully staged
        SecondaryHunkStatus::NoSecondaryHunk
    } else if overlapping.iter().all(|h| h.buffer_range == hunk.buffer_range) {
        // Change exists in both diffs identically → fully unstaged
        SecondaryHunkStatus::HasSecondaryHunk
    } else {
        // Partial overlap → partially staged
        SecondaryHunkStatus::OverlapsWithSecondaryHunk
    }
}

fn ranges_overlap(a: &Range<u32>, b: &Range<u32>) -> bool {
    a.start < b.end && b.start < a.end
}
```

#### Usage pattern

When `GitStore` creates buffer diffs, it links the unstaged diff to the uncommitted diff:

```rust
// unstaged_diff = working tree vs index (base = index text)
// uncommitted_diff = working tree vs HEAD (base = HEAD text)
unstaged_diff.update(cx, |diff, cx| {
    diff.set_secondary_diff(uncommitted_diff.downgrade(), cx);
});
```

Now each hunk in the unstaged diff has a `secondary_status` indicating:
- `NoSecondaryHunk` — the change is staged (in index but matches HEAD)
- `HasSecondaryHunk` — the change is unstaged (differs from both index and HEAD identically)
- `OverlapsWithSecondaryHunk` — the change is partially staged

**Success Criteria:**

- [ ] `SecondaryHunkStatus` enum is defined.
- [ ] `DiffHunk` includes `secondary_status` field.
- [ ] `BufferDiff` supports linking a secondary diff via `set_secondary_diff()`.
- [ ] `compute_secondary_status()` correctly classifies hunks.
- [ ] Recalculation reads secondary diff hunks and annotates primary hunks.
- [ ] `cargo check` passes.

**Design Considerations:**

- The secondary diff is held as `WeakEntity` to avoid circular reference ownership.
- If the secondary diff is dropped, `secondary_status` defaults to `HasSecondaryHunk` (assume unstaged).
- Recalculation of secondary status happens during the existing diff recalculation — no separate pass needed.
- The algorithm compares hunk *positions*, not content — this is sufficient because both diffs share the same current buffer text.

**Test Guidance:**

- Primary category: Unit tests
- File with all changes unstaged: all hunks have `HasSecondaryHunk`.
- File with all changes staged: all hunks have `NoSecondaryHunk`.
- File with some lines staged and others not: hunks have mixed status including `OverlapsWithSecondaryHunk`.
- `ranges_overlap()` edge cases: adjacent ranges don't overlap, identical ranges do.
- Boundary: Secondary diff is `None` (no secondary linked) — all hunks default to `HasSecondaryHunk`.
- Boundary: Secondary diff entity is dropped (WeakEntity returns None) — same default.
- Boundary: Empty secondary hunks list → all primary hunks are `NoSecondaryHunk`.
- Boundary: Single-line hunks at file boundaries.
- Infrastructure: GPUI test harness, two `BufferDiff` entities with different base texts but same buffer.
