# macOS App Foundation

| Field        | Value       |
| ------------ | ----------- |
| **Type**     | `feature`   |
| **Priority** | `p1`        |
| **Parent**   | —           |
| **Created**  | 2026-03-13  |

## Summary

Get a styled, interactive GPUI window on screen with a functional git panel. This covers the initialization prerequisites identified in the [macOS app epic](../.epic/macos_app.md) and the first real UI component — a git panel that displays changed files, supports staging/unstaging, and allows committing.

## Current State

The git abstraction layer (`src/git/`) is substantially implemented and already uses GPUI entities (`Entity<Repository>`, `Entity<GitStore>`) with an async job queue via `cx.spawn()` and `tokio::sync::mpsc`. The app foundation (dependency pinning, assets, entry point, theme, keybindings) is implemented and compiles. The window opens with a placeholder view.

## User Story

As a cupel developer, I want a working GPUI window with a git panel that shows changed files, supports staging, and allows committing so that the app has its first meaningful interactive feature.

## Dependency Graph

```
#1 (pin gpui) → #2 (assets/fonts) → #3 (entry point/window)
                                          ├── #4 (theme system)
                                          └── #5 (actions/keybindings)
                                                      │
                                          #6 (git panel scaffolding)
                                            │
                                          #7 (staging interactions)
                                            │
                                          #8 (commit message/workflow)
                                            │
                                          #9 (panel toolbar)
```

## Success Criteria

- [x] GPUI is pinned in `Cargo.toml` with `rust-embed` and `anyhow` added.
- [x] At least one font is embedded via `rust-embed` and served by an `AssetSource` implementation.
- [x] `main.rs` boots the app, opens a window, and renders a root view.
- [x] A `Theme` struct is registered as a `Global` with a dark palette.
- [x] At least `Quit` (cmd-q) is bound via the `actions!` macro.
- [ ] `AppView` owns an `Entity<GitStore>` and renders a `GitPanel` child.
- [ ] Changed files are listed, grouped by staging state (staged / unstaged).
- [ ] Files can be staged/unstaged via click and keyboard shortcuts.
- [ ] A commit message input and commit button are present and functional.
- [ ] The toolbar displays the current branch name and changed file counts.
- [ ] The app compiles, launches, and renders a styled, interactive git panel.

## Design Considerations

- Follow the patterns described in [.epic/macos_app.md](../.epic/macos_app.md) and [.epic/git.md](../.epic/git.md).
- GPUI is not a stable public API — pin to a specific version and treat updates as deliberate maintenance.
- The existing git entities already use `cx.spawn()` with `tokio` channels for background work — no additional executor setup needed.
- `GitPanel` subscribes to `GitStoreEvent` and re-renders when statuses change.
- The panel reads `Repository` state via `repo.read(cx)` — never calls `git2` directly.

## Out of Scope

- ProjectDiff view (full diff viewer) — separate epic.
- ConflictView — separate epic.
- Multiple windows or split views.
- Light theme or theme switching.
- Hunk-level staging (uses `set_index_text`; complex UI).
- Push/pull execution from toolbar (display-only for now).

## Reference

- Architecture reference: [.epic/macos_app.md](../.epic/macos_app.md)
- Git architecture reference: [.epic/git.md](../.epic/git.md)

---

## Foundation — Sequential

### Pin GPUI Dependency

| Field        | Value       |
| ------------ | ----------- |
| **Type**     | `debt`      |
| **Priority** | `p1`        |
| **Created**  | 2026-03-13  |

**Summary:** Pin the `gpui` crate to a specific Zed monorepo commit (currently declared as `gpui = { version = "*" }`) and add the supporting dependencies (`rust-embed`, `anyhow`) required before asset loading and the app entry point can be built.

**Detailed Description:**

Update `Cargo.toml`:

```toml
[dependencies]
gpui = { git = "https://github.com/zed-industries/zed", rev = "<commit-sha>" }
rust-embed = "8"
anyhow = "1"
```

Steps:

1. Identify a recent stable commit on Zed's `main` branch where `gpui` compiles cleanly on macOS.
2. Replace `gpui = { version = "*" }` with a pinned git dep.
3. Add `rust-embed` and `anyhow` as direct dependencies.
4. Verify `tokio` version compatibility (already in use for the job queue).
5. Run `cargo check` to verify the dependency tree resolves and compiles.
6. Commit the updated `Cargo.toml` and `Cargo.lock`.

Current `Cargo.toml` state:
- `gpui = { version = "*" }` — unpinned, needs to be changed to a git dep with a specific `rev`
- `tokio` — used by the `Repository` entity for its async job queue
- `git2`, `imara-diff`, `thiserror`, `core-text` — existing deps to preserve

**Success Criteria:**

- [ ] `gpui` is declared as a git dependency pinned to a specific `rev` (not `main`, not a branch, not `version = "*"`).
- [ ] `rust-embed` and `anyhow` are in `[dependencies]`.
- [ ] All existing dependencies (`git2`, `imara-diff`, `tokio`, `thiserror`, `core-text`) still compile.
- [ ] `cargo check` passes with all dependencies resolved.
- [ ] `Cargo.lock` is committed with the resolved dependency tree.

**Design Considerations:**

- Never track `main` — GPUI is an internal API that breaks without notice.
- Choose a commit that is recent enough to have the `Context<T>` + `Window` API (post-refactor).
- The project uses `tokio` (not `smol`) for its async job queue — ensure no conflicts with GPUI's internal executor.
- Keep existing dependencies intact.

**Test Guidance:**

- Primary category: Build verification
- `cargo check` passes with all new and existing dependencies.
- Existing git module code (`src/git/`) still compiles without changes.
- Ensure existing `git2`, `imara-diff`, and `tokio` dependencies still compile.
- Infrastructure: macOS with Xcode command line tools (for Metal framework headers).

### Asset Pipeline and Fonts

| Field        | Value       |
| ------------ | ----------- |
| **Type**     | `feature`   |
| **Priority** | `p1`        |
| **Created**  | 2026-03-13  |

**Summary:** Create the `assets/` directory structure, embed at least one font using `rust-embed`, and implement the `AssetSource` trait so GPUI can load fonts and render text.

**Detailed Description:**

#### Directory structure

```
assets/
└── fonts/
    └── <chosen-font>.ttf (or .otf)
```

#### `AssetSource` implementation

Create `src/assets.rs`:

```rust
use gpui::{AssetSource, SharedString};
use rust_embed::RustEmbed;
use std::borrow::Cow;

#[derive(RustEmbed)]
#[folder = "assets/"]
pub struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<Cow<[u8]>>> {
        Self::get(path)
            .map(|f| Some(f.data))
            .ok_or_else(|| anyhow::anyhow!("asset not found: {path}"))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        Ok(Self::iter()
            .filter(|p| p.starts_with(path))
            .map(SharedString::from)
            .collect())
    }
}
```

Wire into the app with `App::new().with_assets(Assets)`.

#### Font selection

Choose a monospace font suitable for a git client UI. Good candidates:
- JetBrains Mono (OFL license)
- Fira Code (OFL license)
- IBM Plex Mono (OFL license)

Include the font license file in `assets/fonts/`.

**Success Criteria:**

- [ ] `assets/fonts/` contains at least one `.ttf` or `.otf` font file and its license.
- [ ] `Assets` struct derives `RustEmbed` with `#[folder = "assets/"]`.
- [ ] `AssetSource` is implemented for `Assets` with working `load()` and `list()` methods.
- [ ] `src/assets.rs` is declared as a module in `src/lib.rs` or `src/main.rs`.
- [ ] `cargo check` passes.

**Design Considerations:**

- GPUI has no fallback system font loader — without an embedded font, nothing renders.
- `rust-embed` bakes assets into the binary at compile time; no runtime file I/O needed.
- Keep the initial font set minimal (one family, regular weight) to limit binary size.
- The `assets/` directory will later hold icons and images for the git UI panels.

**Test Guidance:**

- Primary category: Integration tests
- `Assets::get("fonts/<fontfile>")` returns `Some(...)`.
- `Assets::iter()` lists the embedded font path.
- `AssetSource::load()` returns font bytes for a valid path.
- `AssetSource::load()` returns an error for a nonexistent path.
- Boundary: Empty path string passed to `list()`.
- Boundary: Path with trailing slash vs. without.

### App Entry Point and Window

| Field        | Value       |
| ------------ | ----------- |
| **Type**     | `feature`   |
| **Priority** | `p1`        |
| **Created**  | 2026-03-13  |

**Summary:** Replace the `todo!()` stub in `main.rs` with a working GPUI app entry point, and implement a root view in `app.rs` that renders in the window.

**Detailed Description:**

#### `src/main.rs`

Replace the current stub with:

```rust
fn main() {
    App::new().with_assets(Assets).run(|cx| {
        // Register globals here (theme, settings — later issues)
        cx.open_window(WindowOptions::default(), |window, cx| {
            cx.new(|cx| AppView::new(window, cx))
        });
    });
}
```

#### `src/app.rs`

```rust
pub struct AppView {
    // Will hold child entities as the app grows
}

impl AppView {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        Self {}
    }
}

impl Render for AppView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .bg(gpui::black())
            .child("cupel")
    }
}
```

#### Window configuration

Start with `WindowOptions::default()`. The window should:
- Open at a reasonable default size.
- Display the root view with a dark background.
- Show "cupel" as placeholder text to verify font rendering works.

**Success Criteria:**

- [ ] `src/main.rs` calls `App::new().with_assets(Assets).run(...)` (replaces `todo!()`).
- [ ] `src/app.rs` defines `AppView` implementing `Render`.
- [ ] `cx.open_window()` opens a window with the root view.
- [ ] The app compiles, launches, and displays a window with visible text.
- [ ] `cargo run` starts the application without panics.

**Design Considerations:**

- The `App::run()` closure never returns — it hands control to the macOS event loop.
- Register globals (theme, settings) in the `run()` closure *before* opening windows.
- Keep `AppView` minimal — it will grow as child panels are added.
- Use `size_full()` so the root view fills the entire window.
- The existing `pub mod app` in `lib.rs` means `AppView` is accessible from `main.rs` via `cupel::app::AppView`.

**Test Guidance:**

- Primary category: Manual verification
- `cargo run` launches a window.
- The window displays text (verifies font pipeline works end-to-end).
- Closing the window exits the process cleanly.
- Boundary: Window resize doesn't panic.
- Infrastructure: macOS with a display (cannot be tested in headless CI).

---

## Integration — Parallel (after entry point)

### Theme System

| Field        | Value       |
| ------------ | ----------- |
| **Type**     | `feature`   |
| **Priority** | `p1`        |
| **Created**  | 2026-03-13  |

**Summary:** Create a `Theme` struct registered as a GPUI `Global` with a dark color palette. This provides a centralized, app-wide color system that all UI components can reference.

**Detailed Description:**

#### `src/theme.rs`

```rust
use gpui::{Hsla, Global};

pub struct Theme {
    pub background:   Hsla,
    pub surface:      Hsla,
    pub text_primary: Hsla,
    pub text_muted:   Hsla,
    pub accent:       Hsla,
    pub border:       Hsla,
}

impl Global for Theme {}

impl Default for Theme {
    fn default() -> Self {
        Self {
            background:   hsla(0.0, 0.0, 0.10, 1.0),  // near-black
            surface:      hsla(0.0, 0.0, 0.14, 1.0),  // slightly lighter
            text_primary: hsla(0.0, 0.0, 0.90, 1.0),  // off-white
            text_muted:   hsla(0.0, 0.0, 0.55, 1.0),  // gray
            accent:       hsla(0.58, 0.7, 0.55, 1.0),  // blue
            border:       hsla(0.0, 0.0, 0.20, 1.0),  // subtle border
        }
    }
}
```

#### Registration

In `main.rs`, inside the `App::run()` closure, before opening the window:

```rust
cx.set_global(Theme::default());
```

#### Usage in render

```rust
let theme = cx.global::<Theme>();
div().bg(theme.background).text_color(theme.text_primary)
```

Update `AppView::render()` to use theme colors instead of hardcoded values.

**Success Criteria:**

- [ ] `src/theme.rs` defines `Theme` with at least 6 color fields.
- [ ] `Theme` implements `Global` and `Default`.
- [ ] `Default` provides a dark palette.
- [ ] `Theme` is registered with `cx.set_global()` before window creation.
- [ ] `AppView::render()` uses `cx.global::<Theme>()` for colors.
- [ ] `cargo check` passes.

**Design Considerations:**

- Use `Hsla` over `Rgba` — GPUI's styling methods work natively with `Hsla`.
- Start with a minimal palette (6 colors). Add fields as UI components demand them.
- Don't build a theme switching system yet — just `Default` for dark mode.
- Zed's `theme` crate is enormous and tightly coupled; build a simple, independent system.

**Test Guidance:**

- Primary category: Unit tests
- `Theme::default()` returns a valid theme with all non-zero alpha values.
- All color fields are within valid HSLA ranges (h: 0-1, s: 0-1, l: 0-1, a: 0-1).
- Boundary: Verify `text_primary` has sufficient contrast against `background` (lightness difference > 0.5).

### Actions and Keybindings

| Field        | Value       |
| ------------ | ----------- |
| **Type**     | `feature`   |
| **Priority** | `p1`        |
| **Created**  | 2026-03-13  |

**Summary:** Define application-level actions using GPUI's `actions!` macro, bind them to keyboard shortcuts, and implement at least a `Quit` handler.

**Detailed Description:**

#### `src/actions.rs`

```rust
use gpui::actions;

actions!(cupel, [Quit]);
```

#### Keybinding registration

In `main.rs`, inside the `App::run()` closure:

```rust
cx.bind_keys([
    KeyBinding::new("cmd-q", Quit, None),
]);
```

#### Action handler

In `AppView::render()`:

```rust
div()
    .on_action(cx.listener(|_this, _: &Quit, _window, cx| {
        cx.quit();
    }))
```

#### Key context

Set a key context on the root element so GPUI's action dispatch knows which scope is active:

```rust
div()
    .key_context("cupel")
    // ...
```

**Success Criteria:**

- [ ] `actions!` macro defines at least `Quit`.
- [ ] `cmd-q` is bound to `Quit` via `cx.bind_keys()`.
- [ ] Pressing cmd-q quits the application.
- [ ] Root view element has a `key_context` set.
- [ ] `cargo check` passes.

**Design Considerations:**

- Register keybindings in the `App::run()` closure, alongside globals.
- Use `None` for the context filter on app-level bindings (they apply globally).
- The `actions!` macro generates zero-sized action structs — add more actions as needed.
- Keep actions in a dedicated module so they can be imported from anywhere.

**Test Guidance:**

- Primary category: Manual verification
- `cargo run` and press cmd-q — app should exit cleanly.
- Verify no panic on quit.
- Boundary: Pressing an unbound key combination — no crash.
- Infrastructure: macOS with a display (cannot test keyboard input in headless CI).

---

## Git Panel — Sequential (after foundation)

### GitPanel Scaffolding and File List

| Field        | Value       |
| ------------ | ----------- |
| **Type**     | `feature`   |
| **Priority** | `p1`        |
| **Created**  | 2026-03-13  |

**Summary:** Create the `GitPanel` GPUI entity that subscribes to `Entity<GitStore>`, renders changed files grouped by staging state (staged vs unstaged), and wire it into `AppView` as a child. This is the first real UI component in the app.

**Detailed Description:**

#### `src/git_panel.rs`

Create a `GitPanel` struct that:

1. Holds an `Entity<GitStore>` reference.
2. Subscribes to `GitStoreEvent` to rebuild its entry list when statuses change.
3. Maintains a sorted, grouped list of entries derived from the active repository's statuses.
4. Implements `Render` to display the file list.

```rust
pub struct GitPanel {
    git_store: Entity<GitStore>,
    staged_entries: Vec<StatusEntry>,
    unstaged_entries: Vec<StatusEntry>,
    _subscriptions: Vec<Subscription>,
}
```

#### Entry grouping

Split `Repository.statuses()` into two lists based on `StatusEntry::staging_state()`:
- **Staged** — `StagingState::Staged` or `StagingState::PartiallyStaged`
- **Unstaged** — `StagingState::Unstaged` or `StagingState::PartiallyStaged`

Note: partially staged files appear in both groups (matching Zed/VSCode behavior).

#### Render layout

```
┌──────────────────────────┐
│  Staged Changes (N)      │
│    M  src/main.rs        │
│    A  src/new_file.rs    │
│  Unstaged Changes (N)    │
│    M  src/lib.rs         │
│    ?  src/scratch.rs     │
└──────────────────────────┘
```

- Section headers with file counts.
- Each entry shows a status indicator (A/M/D/?) and the file path.
- Use theme colors: `text_primary` for paths, `accent` for added, `text_muted` for section headers.
- Status indicator colors: green for added, yellow for modified, red for deleted, gray for untracked.

#### Wire into AppView

`AppView` needs to:
1. Create and own an `Entity<GitStore>` in its constructor.
2. Create and own an `Entity<GitPanel>` that references the store.
3. Render the `GitPanel` as a child element.

For initial development, `AppView::new()` should also open a repository at the current working directory using `RealGitRepository` and add it to the store.

#### Update `src/lib.rs`

Add `pub mod git_panel;` to the module declarations.

**Success Criteria:**

- [ ] `src/git_panel.rs` defines `GitPanel` implementing `Render`.
- [ ] `GitPanel` subscribes to `GitStoreEvent` and rebuilds entries on status changes.
- [ ] Changed files are grouped into "Staged" and "Unstaged" sections.
- [ ] Each entry displays a status indicator and file path.
- [ ] `AppView` owns `Entity<GitStore>` and `Entity<GitPanel>`.
- [ ] `AppView` initializes the store with the current working directory's repository.
- [ ] The panel renders in the window with themed colors.
- [ ] `cargo check` passes.

**Design Considerations:**

- `GitPanel` reads `Repository` state via `repo.read(cx)` — never calls `git2` directly.
- The subscription pattern mirrors Zed's approach: subscribe in the constructor, rebuild entries in the handler, call `cx.notify()` to trigger re-render.
- Keep the entry list as simple `Vec<StatusEntry>` for now — no virtualized list until performance demands it.
- Partially staged files appear in both groups so the user can see and act on both aspects.
- `RealGitRepository::open()` for initial repo — will need a `Mutex` wrapper since `git2::Repository` is not `Sync`.

**Test Guidance:**

- Primary category: Integration tests
- Entry grouping logic: given a set of `StatusEntry` values with various staging states, verify correct grouping into staged/unstaged.
- Partially staged files appear in both groups.
- Empty repository produces empty lists.
- Boundary: All files staged, all files unstaged, mix of both.
- Boundary: No active repository (store is empty).
- Boundary: Repository with no changes.
- Infrastructure: `FakeGitRepository` for testing without a real repo, GPUI test harness.

### Staging Interactions

| Field        | Value       |
| ------------ | ----------- |
| **Type**     | `feature`   |
| **Priority** | `p1`        |
| **Created**  | 2026-03-13  |

**Summary:** Add staging and unstaging interactions to the GitPanel: click-to-toggle on individual files, stage-all / unstage-all actions, and keyboard navigation with shortcuts.

**Detailed Description:**

#### Actions

Add to `src/actions.rs`:

```rust
actions!(cupel, [
    Quit,
    StageFile,
    UnstageFile,
    StageAll,
    UnstageAll,
    ToggleStaging,
]);
```

#### Keybindings

Register in `main.rs`:

```rust
cx.bind_keys([
    KeyBinding::new("cmd-q", Quit, None),
    KeyBinding::new("enter", ToggleStaging, Some("git_panel")),
    KeyBinding::new("cmd-shift-a", StageAll, Some("git_panel")),
    KeyBinding::new("cmd-shift-u", UnstageAll, Some("git_panel")),
]);
```

#### Click-to-toggle

Each file entry in the `GitPanel` render gets an `.on_click()` handler:

- Clicking a file in the **Unstaged** section calls `repo.stage([path])`.
- Clicking a file in the **Staged** section calls `repo.unstage([path])`.

The `Repository` entity already handles optimistic UI updates via `pending_ops`.

#### Stage all / Unstage all

- `StageAll`: collect all unstaged file paths, call `repo.stage(paths)`.
- `UnstageAll`: collect all staged file paths, call `repo.unstage(paths)`.

#### Selected entry tracking

Add `selected_index: Option<usize>` to `GitPanel` for keyboard navigation:

- `up` / `down` to move selection.
- `enter` (ToggleStaging) to stage/unstage the selected file.
- Visual highlight on the selected entry using `theme.surface` background.

#### Optimistic UI

The `Repository` entity already tracks `pending_ops`. Use `repo.effective_staging_state(path)` when rendering to show the optimistic state. Optionally show a muted/dimmed style for entries with pending operations.

#### Key context

Set `.key_context("git_panel")` on the GitPanel's root element so that git-panel-specific keybindings only fire when the panel is focused.

**Success Criteria:**

- [ ] Clicking an unstaged file stages it.
- [ ] Clicking a staged file unstages it.
- [ ] `StageAll` and `UnstageAll` actions work via keyboard shortcuts.
- [ ] `ToggleStaging` on a selected entry stages or unstages it.
- [ ] Keyboard navigation (up/down) moves the selection.
- [ ] Selected entry has a visual highlight.
- [ ] Optimistic UI is reflected — pending ops show immediately before the git operation completes.
- [ ] Key context `git_panel` scopes the keybindings correctly.
- [ ] `cargo check` passes.

**Design Considerations:**

- The `Repository::stage()` and `Repository::unstage()` methods already handle optimistic state and background dispatch. The panel just calls them and re-renders on `GitStoreEvent::StatusesChanged`.
- Use `effective_staging_state()` instead of raw `staging_state()` for rendering to account for in-flight operations.
- Focus management: the GitPanel needs a `FocusHandle` so it can receive keyboard events. Use `cx.focus_handle()` and `.track_focus(&focus_handle)` on the root div.
- Keep the selection index simple for now — a flat index across both sections.

**Test Guidance:**

- Primary category: Integration tests
- Staging a file moves it from unstaged to staged group on re-render.
- Unstaging a file moves it from staged to unstaged group.
- StageAll stages all unstaged files.
- UnstageAll unstages all staged files.
- Selection wraps or clamps at list boundaries.
- Boundary: Toggle on a partially staged file.
- Boundary: Stage/unstage when the list is empty.
- Boundary: Action dispatched when panel is not focused (should not fire).
- Infrastructure: `FakeGitRepository`, GPUI test harness for simulating clicks and key presses.

### Commit Message and Workflow

| Field        | Value       |
| ------------ | ----------- |
| **Type**     | `feature`   |
| **Priority** | `p1`        |
| **Created**  | 2026-03-13  |

**Summary:** Add a commit message text input and commit action to the GitPanel. When the user types a message and triggers commit, the staged changes are committed and the message input is cleared.

**Detailed Description:**

#### Commit message input

Add a text input area at the bottom of the GitPanel, above or below the file list. This can be a simple single-line input for the first iteration.

GPUI does not ship a built-in text input widget. Options:
1. **Simple approach**: Use a `String` field + `.on_key_down()` to capture typing. Render the string as text with a cursor indicator.
2. **Better approach**: Check if GPUI provides an `InputElement` or `TextInput` component in its elements. If so, use it.

For MVP, a simple editable string with basic key handling (typing, backspace, enter-to-commit) is sufficient.

#### Commit action

Add to `src/actions.rs`:

```rust
actions!(cupel, [
    Quit,
    StageFile, UnstageFile, StageAll, UnstageAll, ToggleStaging,
    Commit,
]);
```

Keybinding:

```rust
KeyBinding::new("cmd-enter", Commit, Some("git_panel")),
```

#### Commit handler

When `Commit` is dispatched:

1. Read the commit message from the input.
2. If the message is empty, do nothing (or show a visual indicator).
3. Call `repo.commit(message, CommitOptions::default(), cx)` on the active repository.
4. The `Repository` entity handles the async commit via its job queue.

#### Clear on success

Subscribe to `GitStoreEvent::CommitCompleted`:

```rust
GitStoreEvent::CommitCompleted { .. } => {
    self.commit_message.clear();
    cx.notify();
}
```

#### Render layout

```
┌──────────────────────────┐
│  Staged Changes (2)      │
│    M  src/main.rs        │
│    A  src/new_file.rs    │
│  Unstaged Changes (1)    │
│    M  src/lib.rs         │
│ ─────────────────────── │
│  [commit message input]  │
│  [Commit]                │
└──────────────────────────┘
```

The commit button can be a styled div with `.on_click()` or just rely on the `cmd-enter` keybinding.

**Success Criteria:**

- [ ] A text input area is visible in the GitPanel for typing commit messages.
- [ ] Typing updates the displayed message.
- [ ] `cmd-enter` triggers the `Commit` action.
- [ ] `Commit` calls `Repository::commit()` with the entered message.
- [ ] The message input is cleared after a successful commit.
- [ ] Empty commit messages are rejected (no commit dispatched).
- [ ] `cargo check` passes.

**Design Considerations:**

- Start simple: a basic text field with key handling. Polish the input experience (multi-line, selection, copy/paste) in a later iteration.
- The commit is async — `Repository::commit()` queues the job and `CommitCompleted` fires when done. Don't block the UI.
- Consider showing a brief "Committed!" feedback or clearing the staged list as visual confirmation.
- The `CommitOptions::default()` is fine for now (no amend, no signoff).

**Test Guidance:**

- Primary category: Integration tests
- Commit action with a non-empty message calls `Repository::commit()`.
- Commit action with an empty message does not call commit.
- Message is cleared after `CommitCompleted` event.
- Commit with no staged files — depends on git behavior (empty commit error).
- Boundary: Very long commit message.
- Boundary: Commit while a previous commit is still in flight.
- Boundary: No active repository.
- Infrastructure: `FakeGitRepository`, GPUI test harness for simulating input and action dispatch.

### Panel Toolbar with Branch Info

| Field        | Value       |
| ------------ | ----------- |
| **Type**     | `feature`   |
| **Priority** | `p1`        |
| **Created**  | 2026-03-13  |

**Summary:** Add a toolbar to the top of the GitPanel that displays the current branch name, changed file counts, and upstream ahead/behind indicators.

**Detailed Description:**

#### Toolbar layout

```
┌──────────────────────────┐
│  ⎇ main  ↑2 ↓1   3 files│
│ ─────────────────────── │
│  Staged Changes (2)      │
│    ...                   │
└──────────────────────────┘
```

#### Data sources

All data comes from the active `Repository` entity, read via `repo.read(cx)`:

- **Branch name**: `repo.snapshot().branch.as_ref().map(|b| &b.name)`
- **Ahead/behind**: `repo.snapshot().branch.as_ref().and_then(|b| b.upstream.as_ref())` → `upstream.ahead`, `upstream.behind`
- **File count**: `repo.statuses().len()`

#### Render details

- Branch name in `text_primary`, truncated if too long.
- Ahead count (↑N) in `accent` color, only shown if > 0.
- Behind count (↓N) in `text_muted` color, only shown if > 0.
- File count on the right side.
- A subtle `border` bottom separating the toolbar from the file list.
- Use `theme.surface` as the toolbar background to visually distinguish it.

#### Update on events

The toolbar re-renders automatically when `GitPanel` re-renders (since it reads from the same entity state). The `GitStoreEvent::BranchChanged` event triggers a notify, which covers branch switches.

**Success Criteria:**

- [ ] Toolbar is rendered at the top of the GitPanel.
- [ ] Current branch name is displayed.
- [ ] Ahead/behind counts are shown when upstream info is available.
- [ ] Changed file count is displayed.
- [ ] Toolbar uses `theme.surface` background with a bottom border.
- [ ] Toolbar updates when the branch changes or statuses update.
- [ ] `cargo check` passes.

**Design Considerations:**

- Keep the toolbar read-only for now. Push/pull buttons are a future enhancement.
- The `RepositorySnapshot` is a cheap clone — no performance concern reading it on every render.
- If there's no active repository, show a placeholder (e.g., "No repository" in `text_muted`).
- If there's no upstream, omit the ahead/behind indicators entirely.

**Test Guidance:**

- Primary category: Unit tests + manual verification
- Toolbar renders branch name from snapshot.
- Ahead/behind indicators are hidden when upstream is `None`.
- Ahead/behind indicators display correct counts.
- File count matches status entry count.
- "No repository" placeholder when store is empty.
- Boundary: Very long branch name (should truncate or ellipsize).
- Boundary: Detached HEAD (branch might be `None`).
- Boundary: Zero ahead/behind (indicators hidden).
- Boundary: Zero changed files.
- Infrastructure: `FakeGitRepository` with configurable branch and upstream state.
