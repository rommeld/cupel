use gpui::{
    div, Context, Entity, FocusHandle, InteractiveElement, IntoElement, ParentElement, Render,
    StatefulInteractiveElement, Styled, Subscription, Window,
};

use crate::actions::{
    Commit, SelectNext, SelectPrev, StageAll, ToggleStaging, UnstageAll,
};
use crate::git::store::{GitStore, GitStoreEvent};
use crate::git::types::{CommitOptions, FileStatus, RepoPath, StagingState, StatusEntry};
use crate::theme::Theme;

// ---------------------------------------------------------------------------
// Entry — a file in the staged or unstaged section
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct PanelEntry {
    status_entry: StatusEntry,
    staged: bool,
}

// ---------------------------------------------------------------------------
// GitPanel
// ---------------------------------------------------------------------------

pub struct GitPanel {
    git_store: Entity<GitStore>,
    entries: Vec<PanelEntry>,
    selected_index: Option<usize>,
    commit_message: String,
    focus_handle: FocusHandle,
    _subscriptions: Vec<Subscription>,
}

impl GitPanel {
    pub fn new(
        git_store: Entity<GitStore>,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();

        let sub = cx.subscribe(&git_store, |this: &mut Self, _store, event, cx| {
            match event {
                GitStoreEvent::StatusesChanged(_)
                | GitStoreEvent::ActiveRepositoryChanged
                | GitStoreEvent::BranchChanged => {
                    this.rebuild_entries(cx);
                    cx.notify();
                }
                GitStoreEvent::CommitCompleted { .. } => {
                    this.commit_message.clear();
                    this.rebuild_entries(cx);
                    cx.notify();
                }
            }
        });

        let mut panel = Self {
            git_store,
            entries: Vec::new(),
            selected_index: None,
            commit_message: String::new(),
            focus_handle,
            _subscriptions: vec![sub],
        };

        panel.rebuild_entries(cx);
        panel
    }

    fn rebuild_entries(&mut self, cx: &mut Context<Self>) {
        self.entries.clear();

        let store = self.git_store.read(cx);
        let Some(repo_entity) = store.active_repository() else {
            return;
        };

        let repo = repo_entity.read(cx);

        for entry in repo.statuses() {
            let effective = repo.effective_staging_state(&entry.repo_path);
            match effective {
                StagingState::Staged => {
                    self.entries.push(PanelEntry {
                        status_entry: entry.clone(),
                        staged: true,
                    });
                }
                StagingState::Unstaged => {
                    self.entries.push(PanelEntry {
                        status_entry: entry.clone(),
                        staged: false,
                    });
                }
                StagingState::PartiallyStaged => {
                    self.entries.push(PanelEntry {
                        status_entry: entry.clone(),
                        staged: true,
                    });
                    self.entries.push(PanelEntry {
                        status_entry: entry.clone(),
                        staged: false,
                    });
                }
            }
        }

        // Sort: staged first, then unstaged; within each group, by path
        self.entries.sort_by(|a, b| {
            b.staged
                .cmp(&a.staged)
                .then_with(|| a.status_entry.repo_path.cmp(&b.status_entry.repo_path))
        });

        // Clamp selection
        if let Some(idx) = self.selected_index {
            if idx >= self.entries.len() {
                self.selected_index = if self.entries.is_empty() {
                    None
                } else {
                    Some(self.entries.len() - 1)
                };
            }
        }
    }

    fn staged_count(&self) -> usize {
        self.entries.iter().filter(|e| e.staged).count()
    }

    fn unstaged_count(&self) -> usize {
        self.entries.iter().filter(|e| !e.staged).count()
    }

    fn with_active_repo(
        &self,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut crate::git::repo_entity::Repository, &mut Context<crate::git::repo_entity::Repository>),
    ) {
        let store = self.git_store.read(cx);
        let Some(repo_entity) = store.active_repository().cloned() else {
            return;
        };
        repo_entity.update(cx, f);
    }

    fn toggle_staging_for(
        &mut self,
        path: RepoPath,
        currently_staged: bool,
        cx: &mut Context<Self>,
    ) {
        if currently_staged {
            self.with_active_repo(cx, |repo, cx| repo.unstage(vec![path], cx));
        } else {
            self.with_active_repo(cx, |repo, cx| repo.stage(vec![path], cx));
        }
    }

    fn stage_all(&mut self, cx: &mut Context<Self>) {
        self.with_active_repo(cx, |repo, cx| {
            let paths: Vec<RepoPath> = self
                .entries
                .iter()
                .filter(|e| !e.staged)
                .map(|e| e.status_entry.repo_path.clone())
                .collect();
            if !paths.is_empty() {
                repo.stage(paths, cx);
            }
        });
    }

    fn unstage_all(&mut self, cx: &mut Context<Self>) {
        self.with_active_repo(cx, |repo, cx| {
            let paths: Vec<RepoPath> = self
                .entries
                .iter()
                .filter(|e| e.staged)
                .map(|e| e.status_entry.repo_path.clone())
                .collect();
            if !paths.is_empty() {
                repo.unstage(paths, cx);
            }
        });
    }

    fn toggle_selected(&mut self, cx: &mut Context<Self>) {
        let Some(idx) = self.selected_index else {
            return;
        };
        let Some(entry) = self.entries.get(idx) else {
            return;
        };
        let path = entry.status_entry.repo_path.clone();
        let staged = entry.staged;
        self.toggle_staging_for(path, staged, cx);
    }

    fn do_commit(&mut self, cx: &mut Context<Self>) {
        let message = self.commit_message.trim().to_string();
        if message.is_empty() {
            return;
        }
        self.with_active_repo(cx, |repo, cx| {
            repo.commit(message, CommitOptions::default(), cx);
        });
    }

    fn select_next(&mut self, cx: &mut Context<Self>) {
        if self.entries.is_empty() {
            return;
        }
        self.selected_index = Some(match self.selected_index {
            None => 0,
            Some(idx) => (idx + 1).min(self.entries.len() - 1),
        });
        cx.notify();
    }

    fn select_prev(&mut self, cx: &mut Context<Self>) {
        if self.entries.is_empty() {
            return;
        }
        self.selected_index = Some(match self.selected_index {
            None => 0,
            Some(idx) => idx.saturating_sub(1),
        });
        cx.notify();
    }

    fn render_section(
        &self,
        staged: bool,
        count: usize,
        label: &str,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> gpui::Div {
        let mut section = div().w_full().flex().flex_col();

        if count == 0 {
            return section;
        }

        section = section.child(
            div()
                .px_2()
                .py_1()
                .text_color(theme.text_muted)
                .child(format!("{label} ({count})")),
        );

        for (global_idx, entry) in self.entries.iter().enumerate() {
            if entry.staged != staged {
                continue;
            }
            let status_char = status_indicator(&entry.status_entry, staged);
            let color = status_color(&entry.status_entry, staged, theme);
            let is_selected = self.selected_index == Some(global_idx);

            let mut row = div()
                .id(global_idx)
                .px_2()
                .py_px()
                .w_full()
                .flex()
                .flex_row()
                .gap_2()
                .cursor_pointer()
                .child(div().text_color(color).min_w_4().child(status_char))
                .child(
                    div()
                        .text_color(theme.text_primary)
                        .child(entry.status_entry.repo_path.to_string()),
                );

            if is_selected {
                row = row.bg(theme.surface);
            }

            let click_path = entry.status_entry.repo_path.clone();
            row = row.on_click(cx.listener(move |this, _event, _window, cx| {
                this.toggle_staging_for(click_path.clone(), staged, cx);
            }));

            section = section.child(row);
        }

        section
    }

    fn handle_key_down(
        &mut self,
        event: &gpui::KeyDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key_char = event.keystroke.key_char.as_deref();
        match key_char {
            Some(ch) if !event.keystroke.modifiers.platform
                && !event.keystroke.modifiers.control =>
            {
                self.commit_message.push_str(ch);
                cx.notify();
            }
            _ => {
                // Handle special keys by key name
                match event.keystroke.key.as_str() {
                    "backspace" if !event.keystroke.modifiers.platform => {
                        self.commit_message.pop();
                        cx.notify();
                    }
                    _ => {}
                }
            }
        }
    }
}

impl Render for GitPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = *cx.global::<Theme>();
        let theme = &theme;
        let focus_handle = self.focus_handle.clone();

        // Read toolbar data
        let store = self.git_store.read(cx);
        let active_repo = store.active_repository();

        let (branch_name, ahead, behind, total_files) = if let Some(repo_entity) = active_repo {
            let repo = repo_entity.read(cx);
            let snapshot = repo.snapshot();
            let name = snapshot
                .branch
                .as_ref()
                .map(|b| b.name.clone())
                .unwrap_or_else(|| "detached".to_string());
            let (a, b) = snapshot
                .branch
                .as_ref()
                .and_then(|br| br.upstream.as_ref())
                .map(|u| (u.ahead.unwrap_or(0), u.behind.unwrap_or(0)))
                .unwrap_or((0, 0));
            let total = repo.statuses().len();
            (name, a, b, total)
        } else {
            ("No repository".to_string(), 0, 0, 0)
        };

        let staged_count = self.staged_count();
        let unstaged_count = self.unstaged_count();

        // Build toolbar
        let mut toolbar = div()
            .w_full()
            .px_2()
            .py_1()
            .bg(theme.surface)
            .border_b_1()
            .border_color(theme.border)
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .child(
                div()
                    .text_color(theme.text_primary)
                    .child(branch_name),
            );

        if ahead > 0 {
            toolbar = toolbar.child(
                div()
                    .text_color(theme.accent)
                    .child(format!("↑{ahead}")),
            );
        }
        if behind > 0 {
            toolbar = toolbar.child(
                div()
                    .text_color(theme.text_muted)
                    .child(format!("↓{behind}")),
            );
        }

        toolbar = toolbar.child(
            div()
                .flex_grow()
                .text_color(theme.text_muted)
                .flex()
                .justify_end()
                .child(format!(
                    "{} file{}",
                    total_files,
                    if total_files == 1 { "" } else { "s" }
                )),
        );

        // Build staged and unstaged sections
        let staged_section = self.render_section(true, staged_count, "Staged Changes", theme, cx);
        let unstaged_section = self.render_section(false, unstaged_count, "Unstaged Changes", theme, cx);

        // Build commit area
        let has_message = !self.commit_message.trim().is_empty();
        let (commit_display, commit_text_color) = if self.commit_message.is_empty() {
            ("Commit message...".to_string(), theme.text_muted)
        } else {
            (self.commit_message.clone(), theme.text_primary)
        };

        let commit_area = div()
            .w_full()
            .border_t_1()
            .border_color(theme.border)
            .flex()
            .flex_col()
            .gap_1()
            .p_2()
            .child(
                div()
                    .px_2()
                    .py_1()
                    .bg(theme.surface)
                    .border_1()
                    .border_color(theme.border)
                    .rounded_sm()
                    .text_color(commit_text_color)
                    .child(commit_display),
            )
            .child(
                div()
                    .id("commit-button")
                    .px_2()
                    .py_1()
                    .bg(if has_message {
                        theme.accent
                    } else {
                        theme.surface
                    })
                    .rounded_sm()
                    .text_color(if has_message {
                        theme.background
                    } else {
                        theme.text_muted
                    })
                    .cursor_pointer()
                    .flex()
                    .justify_center()
                    .child("Commit")
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.do_commit(cx);
                    })),
            );

        // Assemble
        div()
            .key_context("git_panel")
            .track_focus(&focus_handle)
            .on_action(cx.listener(|this, _: &ToggleStaging, _window, cx| {
                this.toggle_selected(cx);
            }))
            .on_action(cx.listener(|this, _: &StageAll, _window, cx| {
                this.stage_all(cx);
            }))
            .on_action(cx.listener(|this, _: &UnstageAll, _window, cx| {
                this.unstage_all(cx);
            }))
            .on_action(cx.listener(|this, _: &Commit, _window, cx| {
                this.do_commit(cx);
            }))
            .on_action(cx.listener(|this, _: &SelectNext, _window, cx| {
                this.select_next(cx);
            }))
            .on_action(cx.listener(|this, _: &SelectPrev, _window, cx| {
                this.select_prev(cx);
            }))
            .on_key_down(cx.listener(Self::handle_key_down))
            .size_full()
            .flex()
            .flex_col()
            .child(toolbar)
            .child(
                div()
                    .flex_grow()
                    .overflow_y_hidden()
                    .flex()
                    .flex_col()
                    .child(staged_section)
                    .child(unstaged_section),
            )
            .child(commit_area)
    }
}

fn status_indicator(entry: &StatusEntry, staged: bool) -> &'static str {
    if staged {
        match entry.status.index_status {
            FileStatus::Added => "A",
            FileStatus::Modified => "M",
            FileStatus::Deleted => "D",
            FileStatus::Untracked => "?",
            FileStatus::Unchanged => " ",
        }
    } else {
        match entry.status.worktree_status {
            FileStatus::Added => "A",
            FileStatus::Modified => "M",
            FileStatus::Deleted => "D",
            FileStatus::Untracked => "?",
            FileStatus::Unchanged => " ",
        }
    }
}

fn status_color(entry: &StatusEntry, staged: bool, theme: &Theme) -> gpui::Hsla {
    let file_status = if staged {
        entry.status.index_status
    } else {
        entry.status.worktree_status
    };

    match file_status {
        FileStatus::Added | FileStatus::Untracked => theme.added,
        FileStatus::Modified => theme.modified,
        FileStatus::Deleted => theme.deleted,
        FileStatus::Unchanged => theme.text_muted,
    }
}
