use std::sync::Arc;

use gpui::{div, AppContext, Context, Entity, IntoElement, ParentElement, Render, Styled, Window};

use crate::git::backend_router::{BackendRouter, GitLocalOps};
use crate::git::gh_cli::GhCliBacked;
use crate::git::real_repo::RealGitRepository;
use crate::git::store::GitStore;
use crate::ui::git_panel::GitPanel;
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
                let work_dir = repo.work_directory().map(|p| p.to_path_buf());
                let repo = Arc::new(repo);

                // Try to set up gh CLI forge backend
                let forge = work_dir.as_ref().and_then(|wd| {
                    GhCliBacked::try_new(wd, None)
                        .map(|gh| Arc::new(gh) as Arc<dyn crate::git::backend_router::GitForgeOps>)
                });

                let router = BackendRouter::new(repo, forge);
                git_store.update(cx, |store, cx| {
                    store.add_repository(router, cx);
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
