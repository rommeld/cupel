use gpui::{AppContext, Application, KeyBinding, WindowOptions};

use cupel::actions::{
    Commit, Quit, SelectNext, SelectPrev, StageAll, ToggleStaging, UnstageAll,
};
use cupel::app::AppView;
use cupel::assets::Assets;
use cupel::theme::Theme;

fn main() {
    Application::new()
        .with_assets(Assets)
        .run(|cx| {
            cx.set_global(Theme::default());

            cx.bind_keys([
                KeyBinding::new("cmd-q", Quit, None),
                KeyBinding::new("enter", ToggleStaging, Some("git_panel")),
                KeyBinding::new("down", SelectNext, Some("git_panel")),
                KeyBinding::new("up", SelectPrev, Some("git_panel")),
                KeyBinding::new("cmd-shift-a", StageAll, Some("git_panel")),
                KeyBinding::new("cmd-shift-u", UnstageAll, Some("git_panel")),
                KeyBinding::new("cmd-enter", Commit, Some("git_panel")),
            ]);

            cx.on_action(|_: &Quit, cx| {
                cx.quit();
            });

            cx.open_window(WindowOptions::default(), |window, cx| {
                cx.new(|cx| AppView::new(window, cx))
            })
            .expect("failed to open window");
        });
}
