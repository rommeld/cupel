//! Several session in one TUI: the origin in the main checkout and its spinoffs, each
//! a complete [`App`] of its own. `Sessions` decides which one is on screen and sends
//! input there, while every session's agent keeps working, on screen or not.

use cupel_agent::AgentMessage;
use cupel_coding_agent::commands::SpinoffCommand;
use cupel_coding_agent::session;
use cupel_coding_agent::spinoff::{
    self, Conflict, Group, MergeOutcome, Side, Spinoff, SpinoffError, SpinoffName,
};
use cupel_core::types::{AssistantMessage, Message};
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
    pub conflicts: Vec<Conflict>,
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
            conflicts: Vec::new(),
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
            SpinoffCommand::Merge { name } => self.merge(&name).await,
            SpinoffCommand::Drop { name } => self.drop_spinoff(&name).await,
        };
        if let Err(error) = result {
            self.active_mut().notice(error.to_string());
        }
    }

    /// `/spinoff`: the origin and every spinoff git knows, and which of them have no
    /// session in this TUI.
    fn list_group(&mut self) -> Result<(), SpinoffError> {
        let group = self.group()?;
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
        let group = self.group()?;
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

    /// After a restart: every spinoff worktree git knows becomes a session again,
    /// continuing its newest transcript. Inside a spinoff's worktree, cupel stays
    /// single session and says so.
    pub async fn restore(&mut self) {
        let group = match self.group() {
            Ok(group) => group,
            Err(SpinoffError::Blocked(message)) => {
                self.list[0].app.notice(message);
                return;
            }
            Err(_) => return,
        };
        if group.spinoffs.is_empty() {
            return;
        }
        for spinoff in &group.spinoffs {
            let app = self.reopen(spinoff).await;
            let label = spinoff.name.clone();
            self.list.push(Session { label, app });
        }
        let names: Vec<&str> = group
            .spinoffs
            .iter()
            .map(|spinoff| spinoff.name.as_str())
            .collect();
        let origin = &mut self.list[0];
        if let Some(branch) = &group.origin.branch {
            origin.label = branch.clone();
        }
        origin.app.notice(format!(
            "spinoffs from the last run: {} - ctrl+n/ctrl+p or a click in the sidebar \
            switches sessions",
            names.join(", ")
        ));
    }

    /// A session for `spinoff` that continues its newest transcript, as `--resume`
    /// does: the same session id, the history on screen and in the context. The model
    /// is the one of the last answer; thinking level and preset prompt come from the
    /// origin, because the transcript records neither.
    async fn reopen(&self, spinoff: &Spinoff) -> App {
        let origin = &self.list[0].app;
        let mut carry = origin.carry();
        let home = origin.meta.home.clone();
        let mut notes = Vec::new();
        let fresh = || (format!("cupel-{}", cupel_core::types::now_ms()), Vec::new());
        let latest = session::find_latest(home.as_deref(), &spinoff.path);
        let (session_id, history) = match latest.map(|path| session::load_transcript(&path)) {
            Some(Ok((header, messages))) => (header.session_id, messages),
            Some(Err(error)) => {
                notes.push(format!("{error} - the session starts empty"));
                fresh()
            }
            None => fresh(),
        };
        if let Some(answer) = last_answer(&history) {
            let model = origin
                .meta
                .models
                .iter()
                .find(|model| model.provider == answer.provider && model.id == answer.model);
            match model {
                Some(model) => carry.model = model.clone(),
                None => notes.push(format!(
                    "{}/{}, the model of the last answer, is not available - the session \
                    uses {}",
                    answer.provider.as_str(),
                    answer.model,
                    carry.model.id,
                )),
            }
        }
        let registry = origin.agent.registry();
        let mut app = App::open(&spinoff.path, home, registry, carry, session_id, history).await;
        for note in notes {
            app.notice(note);
        }
        app
    }

    /// `/spinoff merge <name>`: the next step of merging spinoff `name` into the
    /// origin's branch. Conflicts go to the origin's model as a prompt; the next
    /// `spinoff merge` finishes the merge and cleans up.
    async fn merge(&mut self, name: &str) -> Result<(), SpinoffError> {
        let (group, spinoff) = self.prepare(name)?;
        let origin = &self.list[0];
        if origin.app.is_running() {
            return Err(SpinoffError::Blocked(format!(
                "{} is still working - merge when it is done",
                origin.label
            )));
        }
        let task = self.first_prompt(&spinoff);
        let message = format!("spinoff {name}: {task}");
        match spinoff::merge(&group, &spinoff, &message)? {
            MergeOutcome::Merged => self.clean_up(&group, &spinoff, true).await,
            MergeOutcome::Conflicts(files) => {
                let log = spinoff::incoming_log(&group)?;
                let base = group.origin.branch.as_deref().unwrap_or_default();
                let prompt = resolution_prompt(name, base, &task, &log, &files);
                let origin = &mut self.list[0].app;
                origin.notice(format!(
                    "merging {name} stopped at conflicts in {} - the model resolves them \
                    now, then /spinoff merge {name} finishes the merge",
                    files.join(", ")
                ));
                origin.send(&prompt);
                Ok(())
            }
            MergeOutcome::StillConflicted(files) => Err(SpinoffError::Blocked(format!(
                "conflict markers are left in {} - ask the model again or fix them by hand, \
                then run /spinoff merge {name}",
                files.join(", ")
            ))),
        }
    }

    /// `/spinoff` drop <name>`: delete the spinoff, and leave the origin's checkout,
    /// branch and history alone. Its leftovers are committed to its branch first, so
    /// the commit in the notice holds all of its work.
    async fn drop_spinoff(&mut self, name: &str) -> Result<(), SpinoffError> {
        let (group, spinoff) = self.prepare(name)?;
        let message = format!("spinoff {name}: {}", self.first_prompt(&spinoff));
        spinoff::commit_leftovers(&spinoff, &message)?;
        self.clean_up(&group, &spinoff, false).await
    }

    /// The group and spinoff `name`, for `/spinoff merge` and `/spinoff drop`. Both
    /// belong to the origin, and neither pulls a worktree from under a working agent.
    fn prepare(&self, name: &str) -> Result<(Group, Spinoff), SpinoffError> {
        let origin = &self.list[0];
        if self.active != 0 {
            return Err(SpinoffError::Blocked(format!(
                "/spinoff merge and /spinoff drop run in {}, the origin session",
                origin.label
            )));
        }
        let group = self.group()?;
        let spinoff = group
            .spinoffs
            .iter()
            .find(|spinoff| spinoff.name == name)
            .cloned()
            .ok_or_else(|| {
                SpinoffError::Blocked(format!("no spinoff named {name} - /spinoff lists them"))
            })?;
        if self
            .index_of(&spinoff)
            .is_some_and(|index| self.list[index].app.is_running())
        {
            return Err(SpinoffError::Blocked(format!(
                "{name} is still working - wait for it, or stop it in its session"
            )));
        }
        Ok((group, spinoff))
    }

    /// The checkouts this TUI works with. Refused when cupel runs inside a spinoff's
    /// worktree: there it is a single session, and spinoffs belong to the main
    /// checkout.
    fn group(&self) -> Result<Group, SpinoffError> {
        let cwd = Path::new(&self.list[0].app.meta.cwd);
        let group = Group::discover(cwd)?;
        if let Some(spinoff) = group
            .spinoffs
            .iter()
            .find(|spinoff| cwd.starts_with(&spinoff.path))
        {
            return Err(SpinoffError::Blocked(format!(
                "cupel runs in spinoff {}'s worktree, as a single session - /spinoff works \
                in the main checkout, {}",
                spinoff.name,
                group.origin.path.display()
            )));
        }
        Ok(group)
    }

    /// The open session in `spinoff`'s worktree, if there is one.
    fn index_of(&self, spinoff: &Spinoff) -> Option<usize> {
        (1..self.list.len())
            .find(|&index| Path::new(&self.list[index].app.meta.cwd) == spinoff.path)
    }

    /// What the spinoff was started for: the first prompt of its oldest transcript.
    fn first_prompt(&self, spinoff: &Spinoff) -> String {
        let home = self.list[0].app.meta.home.as_deref();
        session::sessions_dir(home, &spinoff.path)
            .and_then(|dir| session::list_sessions_in(&dir).pop())
            .map(|oldest| oldest.label)
            .filter(|label| !label.is_empty())
            .unwrap_or_else(|| "(no prompt)".to_string())
    }

    /// Close the spinoff's session, remove its worktree and branch, then archive its
    /// transcript . In this order, the `session-end` hook still findes the worktree,
    /// and nothing is archived when a removal fails.
    async fn clean_up(
        &mut self,
        group: &Group,
        spinoff: &Spinoff,
        merged: bool,
    ) -> Result<(), SpinoffError> {
        if let Some(index) = self.index_of(spinoff) {
            let mut closed = self.list.remove(index);
            closed.app.recorder.end_session().await;
        }
        let tip = spinoff::remove(group, spinoff, merged)?;
        let side = Side::Spinoff(spinoff.name.clone());
        self.conflicts
            .retain(|conflict| conflict.other(&side).is_none());

        let home = self.list[0].app.meta.home.clone();
        let into = &group.origin.path;
        let archived = match session::archive_sessions(home.as_deref(), &spinoff.path, into) {
            Ok(count) => format!("transcripts archived: {count}"),
            Err(error) => format!("transcripts not archived: {error}"),
        };
        let name = &spinoff.name;
        let notice = if merged {
            let base = group.origin.branch.as_deref().unwrap_or_default();
            format!("spinoff {name} is merged into {base}: worktree and branch removed, {archived}")
        } else {
            let commit = tip.get(..7).unwrap_or(&tip);
            format!(
                "spinoff {name} is dropped: worktree and branch removed, {archived} - git \
                branch {}{name} {commit} brings its work back",
                spinoff::BRANCH_PREFIX
            )
        };
        self.list[0].app.notice(notice);
        Ok(())
    }

    /// Whether a run ended in any session since the last call. It clears the flags, so
    /// every end counts once.
    pub fn take_finished_runs(&mut self) -> bool {
        let mut any = false;
        for session in &mut self.list {
            any |= core::mem::take(&mut session.app.run_finished);
        }
        any
    }

    /// Check all checkouts of the group for conflicts. The sidebar shows the result,
    /// and every open session whose conflicts changed gets a notice. Withput a spinoff
    /// session there is nothing to compare. A failing check is only logged.
    pub async fn check_conflicts(&mut self) {
        if self.list.len() < 2 {
            return;
        }
        let origin_dir = PathBuf::from(&self.list[0].app.meta.cwd);
        let result = tokio::task::spawn_blocking(move || {
            let group = Group::discover(&origin_dir)?;
            spinoff::check(&group)
        })
        .await;
        let found = match result {
            Ok(Ok(found)) => found,
            Ok(Err(error)) => {
                tracing::warn!("conflict check failed: {error}");
                return;
            }
            Err(error) => {
                tracing::warn!("conflict check did not finish: {error}");
                return;
            }
        };
        let before = core::mem::replace(&mut self.conflicts, found);
        for index in 0..self.list.len() {
            for notice in self.changes(index, &before) {
                self.list[index].app.notice(notice);
            }
        }
    }

    /// The notices for session `index`.
    fn changes(&self, index: usize, before: &[Conflict]) -> Vec<String> {
        let side = self.side(index);
        let old = conflicts_of(before, &side);
        let new = conflicts_of(&self.conflicts, &side);
        let mut notices = Vec::new();
        for pair in &new {
            if !old.contains(pair) {
                let (other, files) = pair;
                let label = self.label(other);
                notices.push(format!("conflict with {label}: {}", files.join(", ")));
            }
        }
        for (other, _) in &old {
            if !new.iter().any(|(side, _)| side == other) {
                notices.push(format!("no more conflicts with {}", self.label(other)));
            }
        }
        notices
    }

    /// Which side of a conflict session `index` is.
    fn side(&self, index: usize) -> Side {
        if index == 0 {
            Side::Origin
        } else {
            Side::Spinoff(self.list[index].label.clone())
        }
    }

    /// The name to show for `side`.
    fn label(&self, side: &Side) -> String {
        match side {
            Side::Origin => self.list[0].label.clone(),
            Side::Spinoff(name) => name.clone(),
        }
    }

    /// How many files of session ìndex`conflicts with any other checkout, for the
    /// sidebar's `!n`.
    #[must_use]
    pub fn conflict_count(&self, index: usize) -> usize {
        let side = self.side(index);
        let mut files: Vec<&String> = conflicts_of(&self.conflicts, &side)
            .into_iter()
            .flat_map(|(_, files)| files)
            .collect();
        files.sort();
        files.dedup();
        files.len()
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

/// The last answer in `history`. It records the provider and the model that gave it.
fn last_answer(history: &[AgentMessage]) -> Option<&AssistantMessage> {
    history.iter().rev().find_map(|message| match message {
        AgentMessage::Llm(Message::Assistant(answer)) => Some(answer),
        _ => None,
    })
}

/// The prompt that hands a stopped merge to the origin's model.
fn resolution_prompt(name: &str, base: &str, task: &str, log: &str, files: &[String]) -> String {
    let branch = format!("{}{name}", spinoff::BRANCH_PREFIX);
    let files = files
        .iter()
        .map(|file| format!("- {file}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "Merging spinoff \"{name}\" (branch {branch}) into {base} stopped with conflicts.\n\
        The spinoff worked on: {task}\n\
        Its commits:\n\
        {log}\
        Conflicted files (HEAD = this session's side, {branch} = the spinoff's side):\n\
        {files}\n\
        Resolve every conflict so that both sides' intent survives, remove all conflict \
        markers, and run the project's checks. Do not commit, and do not run git merge, \
        checkout or reset: cupel finishes the merge when the user runs /spinoff merge \
        {name} again."
    )
}

fn conflicts_of<'a>(conflicts: &'a [Conflict], side: &Side) -> Vec<(&'a Side, &'a [String])> {
    conflicts
        .iter()
        .filter_map(|conflict| Some((conflict.other(side)?, conflict.files.as_slice())))
        .collect()
}
