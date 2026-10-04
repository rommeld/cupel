//! Several session in one TUI: the origin in the main checkout and its spinoffs, each
//! a complete [`App`] of its own. `Sessions` decides which one is on screen and sends
//! input there, while every session's agent keeps working, on screen or not.

use cupel_coding_agent::commands::SpinoffCommand;
use cupel_coding_agent::spinoff::{self, Group, SpinoffError, SpinoffName};
use futures_util::future::select_all;
use ratatui::crossterm::event::{
    Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use ratatui::layout::{Position, Rect};
use std::path::{Path, PathBuf};

use crate::app::{App, AppEvent, ReloadTarget};

pub struct Session {
    pub label: String,
    pub app: App,
}

pub struct Sessions {
    pub list: Vec<Session>,
    pub active: usize,
    pub rows: Vec<(Rect, usize)>,
    pub quit_armed: bool,
}

impl Sessions {
    #[must_use]
    pub fn new(origin: App) -> Self {
        Self {
            list: vec![Session {
                label: "origin".to_string(),
                app: origin,
            }],
            active: 0,
            rows: Vec::new(),
            quit_armed: false,
        }
    }

    #[must_use]
    pub fn active(&self) -> &App {
        &self.list[self.active].app
    }

    pub fn active_mut(&mut self) -> &mut App {
        &mut self.list[self.active].app
    }

    /// Input goes to the session on screen, except the keys and clicks that switch
    /// sessions.
    pub fn on_terminal_event(&mut self, event: Event) {
        match event {
            Event::Key(key)
                if key.kind == KeyEventKind::Press
                    && key.modifiers.contains(KeyModifiers::CONTROL)
                    && matches!(key.code, KeyCode::Char('n' | 'p')) =>
            {
                // Backwards is "len - 1 forwards": the index never goes below 0.
                let step = if key.code == KeyCode::Char('n') {
                    1
                } else {
                    self.list.len() - 1
                };
                self.switch_to((self.active + step) % self.list.len());
            }
            Event::Mouse(mouse) if mouse.kind == MouseEventKind::Down(MouseButton::Left) => {
                let at = Position {
                    x: mouse.column,
                    y: mouse.row,
                };
                let hit = self
                    .rows
                    .iter()
                    .find(|(row, _)| row.contains(at))
                    .map(|&(_, index)| index);
                match hit {
                    Some(index) => self.switch_to(index),
                    None => self.active_mut().on_terminal_event(event),
                }
            }
            _ => self.active_mut().on_terminal_event(event),
        }
    }

    fn switch_to(&mut self, index: usize) {
        let captured = self.active().mouse_captured;
        self.active = index;
        self.active_mut().mouse_captured = captured;
    }

    /// A session off screen must be read too: its transcript file and its hooks only
    /// move on when its events are handled.
    pub async fn next_event(&mut self) -> (usize, AppEvent) {
        let waits = self.list.iter_mut().enumerate().map(|(index, session)| {
            Box::pin(async move { (index, session.app.next_event().await) })
        });
        select_all(waits).await.0
    }

    #[must_use]
    pub fn any_running(&self) -> bool {
        self.list.iter().any(|session| session.app.is_running())
    }

    /// A spinner for each session.
    pub fn tick(&mut self) {
        for session in &mut self.list {
            session.app.tick();
        }
    }

    /// `/new` and `/hot-reload` in the session on screen, rebuild that one `App` and
    /// put it back in its place.
    pub async fn reload(&mut self, target: ReloadTarget) {
        let Session { label, app } = self.list.remove(self.active);
        let app = app.hot_reload(target).await;
        self.list.insert(self.active, Session { label, app });
    }

    /// The session on screen asked to quit (Ctrl+C, Ctrl+D, `/quit`). Quit, unless
    /// another session is still working: then warn once, and quit on the next attempt.
    pub fn quit_requested(&mut self) -> bool {
        if !core::mem::take(&mut self.active_mut().should_quit) {
            return false;
        }
        let busy: Vec<String> = self
            .list
            .iter()
            .enumerate()
            .filter(|(index, session)| *index != self.active && session.app.is_running())
            .map(|(_, session)| session.label.clone())
            .collect();
        if busy.is_empty() || self.quit_armed {
            return true;
        }
        self.quit_armed = true;
        self.active_mut().notice(format!(
            "{} still working - quit again to abort and exit",
            busy.join(", ")
        ));
        false
    }

    /// Stop every session by aborting running agents, wait for them, and let each
    /// recorder announce its end (the `session-end` hook).
    pub async fn shutdown(&mut self) {
        for session in &mut self.list {
            if session.app.is_running() {
                session.app.agent.abort();
            }
            session.app.agent.wait_for_idle().await;
            session.app.recorder.end_session().await;
        }
    }

    /// Carry out a `/spinoff` command from the session on screen. Failures become a
    /// notice there.
    pub async fn spinoff(&mut self, command: SpinoffCommand) {
        let result = match command {
            SpinoffCommand::List => self.list_group(),
            SpinoffCommand::Create { name, preset } => self.create(&name, preset.as_deref()).await,
        };
        if let Err(error) = result {
            self.active_mut().notice(error.to_string());
        }
    }

    /// `/spinoff`: the origin and every spinoff git knows, and which of them have no
    /// session in this TUI.
    fn list_group(&mut self) -> Result<(), SpinoffError> {
        let group = Group::discover(Path::new(&self.list[0].app.meta.cwd))?;
        let branch = group.origin.branch.as_deref().unwrap_or("a detached HEAD");
        let mut lines = vec![format!(
            "origin: {} on {branch}",
            group.origin.path.display()
        )];
        if group.spinoffs.is_empty() {
            lines.push("no spinoffs yet. start one with /spinoff <name> [preset]".to_string());
        }
        for spinoff in &group.spinoffs {
            let open = self
                .list
                .iter()
                .any(|session| spinoff.path == Path::new(&session.app.meta.cwd));
            let note = if open { "" } else { " (no session open)" };
            lines.push(format!(
                " {} {}{note}",
                spinoff.name,
                spinoff.path.display()
            ));
        }
        self.active_mut().notice(lines.join("\n"));
        Ok(())
    }

    /// `/spinoff <name> [preset]`: a worktree for the spinoff and a new session in it,
    /// shown right away.
    async fn create(
        &mut self,
        name: &SpinoffName,
        preset: Option<&str>,
    ) -> Result<(), SpinoffError> {
        let requester = self.active();
        let mut carry = requester.carry();
        if let Some(preset_name) = preset {
            let preset = requester
                .meta
                .settings
                .presets
                .get(preset_name)
                .ok_or_else(|| {
                    SpinoffError::Blocked(format!(
                        "unknown preset: {preset_name} (/preset lists them)"
                    ))
                })?;
            carry.model = preset
                .find_model(&requester.meta.models)
                .cloned()
                .ok_or_else(|| {
                    SpinoffError::Blocked(format!(
                        "preset {preset_name}: unknown model {}/{}",
                        preset.provider, preset.model
                    ))
                })?;
            carry.thinking = preset.thinking_level.level();
            carry.preset_prompt =
                cupel_coding_agent::system_prompt::with_preset_prompt("", preset.prompt.as_deref());
        }
        let home = requester.meta.home.clone();
        let registry = requester.agent.registry();

        let origin_dir = PathBuf::from(&self.list[0].app.meta.cwd);
        let group = Group::discover(&origin_dir)?;
        let spinoff = spinoff::create(&group, name)?;
        let trust_warning = copy_trust(home.as_deref(), &origin_dir, &spinoff.path);
        let session_id = format!("cupel-{}", cupel_core::types::now_ms());
        let mut app = App::open(&spinoff.path, home, registry, carry, session_id, Vec::new()).await;

        // `create` refuses a detached HEAD, so the origin has a branch.
        let base = group.origin.branch.unwrap_or_default();
        let commit = spinoff.head.get(..7).unwrap_or(&spinoff.head);
        app.notice(format!(
            "spinoff {}: worktree {}, branch {}{} from {base} at {commit} (uncommitted \
            changes of the origin are not in it)",
            spinoff.name,
            spinoff.path.display(),
            spinoff::BRANCH_PREFIX,
            spinoff.name,
        ));
        if let Some(warning) = trust_warning {
            app.notice(warning);
        }
        self.active_mut().notice(format!(
            "spinoff {} started - ctrl+n/ctrl+p or a click in the sidebar switches sessions",
            spinoff.name
        ));
        self.list[0].label = base;
        self.list.push(Session {
            label: spinoff.name,
            app,
        });
        self.switch_to(self.list.len() - 1);
        Ok(())
    }
}

/// Gice `worktree` the trust decision made for the origin's directory, so the spinoff
/// runs with the same project hooks and model rows: the trust popup only asks at
/// startup. Returns a warning when saving fails; the spinoff then runs restricted,
/// which is safe.
fn copy_trust(home: Option<&Path>, origin: &Path, worktree: &Path) -> Option<String> {
    let home = home?;
    let decision = cupel_coding_agent::project_trust::decision(Some(home), origin)?;
    cupel_coding_agent::project_trust::save(home, worktree, decision)
        .err()
        .map(|error| format!("warning: the spinoff runs without project trust ({error})"))
}
