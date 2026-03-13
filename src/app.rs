use std::sync::Arc;

use gpui::{div, AppContext, Context, Entity, IntoElement, ParentElement, Render, Styled, Window};

use crate::git::real_repo::RealGitRepository;
use crate::git::store::GitStore;
use crate::git_panel::GitPanel;
use crate::theme::Theme;

pub struct AppView {
    _git_store: Entity<GitStore>,
    git_panel: Entity<GitPanel>,
}

impl AppView {
    pub fn new(_window: &mut Window, cx: &mut Context<Self>) -> Self {
        let git_store = cx.new(|cx| GitStore::new(cx));

        // Open the repo at the current working directory
        if let Ok(cwd) = std::env::current_dir() {
            if let Ok(repo) = RealGitRepository::discover(&cwd) {
                git_store.update(cx, |store, cx| {
                    store.add_repository(Arc::new(repo), cx);
                });
            }
        }

        let store_for_panel = git_store.clone();
        let git_panel = cx.new(|cx| GitPanel::new(store_for_panel, cx));

        Self {
            _git_store: git_store,
            git_panel,
        }
    }
}

impl Render for AppView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.global::<Theme>();

        div()
            .size_full()
            .bg(theme.background)
            .text_color(theme.text_primary)
            .child(self.git_panel.clone())
    }
}
