//! Updates in the background: the check at startup, and `/update`.
//!
//! The work runs in a spawned task that reports over a channel, the way a login does
//! (login.rs). Unlike a login it belongs to the whole process, not to one session:
//! the event loop owns the one [`Updates`], so `/new`, `/hot-reload` and switching
//! sessions never start a second check, its notice comes once, and two `/update`s
//! can't run side by side.

use std::path::PathBuf;

use cupel_coding_agent::update::{self, Outcome, UpdateError, Version};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::sessions::{Restart, Sessions};

/// What an update task reports.
pub enum UpdateEvent {
    Checked(Option<Version>),
    Progress(String),
    Done(Result<Outcome, UpdateError>),
}

/// What the update work does right now. The states exclude each other, so they are one
/// enum and not a set of flags that could contradict each other.
enum State {
    Idle,
    Checking(JoinHandle<()>),
    Updating(JoinHandle<()>),
    /// The release on disk; this process still runs the old one.
    Installed {
        version: Version,
        binary: PathBuf,
    },
}

/// The update work of this process, owned by the event loop.
pub struct Updates {
    sender: mpsc::UnboundedSender<UpdateEvent>,
    events: mpsc::UnboundedReceiver<UpdateEvent>,
    state: State,
    url: String,
}

impl Default for Updates {
    fn default() -> Self {
        let (sender, events) = mpsc::unbounded_channel();
        Self {
            sender,
            events,
            state: State::Idle,
            url: update::LATEST_RELEASE_URL.to_string(),
        }
    }
}

impl Updates {
    /// Ask GitHub in the background whether a newer release is out. A failure (offline,
    /// a timeout, the rate limit) only reaches the log: the check is a courtesy and
    /// must never get in the way.
    pub fn check(&mut self) {
        let sender = self.sender.clone();
        let url = self.url.clone();
        let task = tokio::spawn(async move {
            let current = update::current_version();
            let newer = match update::check(&update::client(), &url, &current).await {
                Ok(release) => release.map(|release| release.version),
                Err(error) => {
                    tracing::warn!("update check failed: {error}");
                    None
                }
            };
            let _ = sender.send(UpdateEvent::Checked(newer));
        });
        self.state = State::Checking(task);
    }

    /// `/update` was typed: install the newest release in the background, unless
    /// something is in the way. The task reports through events, like the check.
    /// After an install whose restart was in the way, `/update` tries the restart
    /// again: a `Restart` asks the event loop to end the TUI and start the new binary.
    pub fn request(&mut self, sessions: &mut Sessions) -> Option<Restart> {
        match &self.state {
            State::Updating(_) => {
                sessions.active_mut().notice("an update is running already");
                return None;
            }
            State::Installed { .. } => return self.restart(sessions),
            State::Idle | State::Checking(_) => {}
        }
        let busy: Vec<&str> = sessions
            .list
            .iter()
            .filter(|session| session.app.is_running())
            .map(|session| session.label.as_str())
            .collect();
        if !busy.is_empty() {
            let text = format!(
                "{} still working - run /update when every session is idle",
                busy.join(", ")
            );
            sessions.active_mut().notice(text);
            return None;
        }
        // `/update` asks GitHub itself, so a check that still runs has nothing to add.
        if let State::Checking(task) = &self.state {
            task.abort();
        }
        sessions
            .active_mut()
            .notice("looking for a newer release...");
        let sender = self.sender.clone();
        let url = self.url.clone();
        let task = tokio::spawn(async move {
            let progress = sender.clone();
            let notify = move |text| {
                let _ = progress.send(UpdateEvent::Progress(text));
            };
            let result = match update::running_binary() {
                Ok(binary) => {
                    let current = update::current_version();
                    update::install(&update::client(), &url, &binary, &current, notify).await
                }
                Err(error) => Err(error),
            };
            let _ = sender.send(UpdateEvent::Done(result));
        });
        self.state = State::Updating(task);
        None
    }

    /// The next event of a task. While there is none, this waits.
    pub async fn next_event(&mut self) -> UpdateEvent {
        match self.events.recv().await {
            Some(event) => event,
            None => std::future::pending().await,
        }
    }

    /// Show what a task reported, in the session on screen. After an install, the
    /// `Restart` onto the new release, unless something is in the way.
    pub fn on_event(&mut self, event: UpdateEvent, sessions: &mut Sessions) -> Option<Restart> {
        match event {
            UpdateEvent::Checked(newer) => {
                // Only the answer of a check that still runs counts: `/update` stops
                // the check, and its answer may be on its way already.
                if !matches!(self.state, State::Checking(_)) {
                    return None;
                }
                self.state = State::Idle;
                if let Some(version) = newer {
                    sessions.active_mut().notice(format!(
                        "cupel {version} is available - you're running {}. Run /update to \
                        install it.",
                        update::current_version(),
                    ));
                }
            }
            UpdateEvent::Progress(text) => sessions.active_mut().notice(text),
            UpdateEvent::Done(Ok(Outcome::UpToDate(newest))) => {
                self.state = State::Idle;
                sessions.active_mut().notice(format!(
                    "nothing to install - you're running {}, the newest release is {newest}",
                    update::current_version()
                ));
            }
            UpdateEvent::Done(Ok(Outcome::Installed { version, binary })) => {
                self.state = State::Installed { version, binary };
                return self.restart(sessions);
            }
            UpdateEvent::Done(Err(error)) => {
                self.state = State::Idle;
                sessions
                    .active_mut()
                    .notice(format!("update failed: {error}"));
            }
        }
        None
    }

    /// Restart onto the release `/update` installed, or say what is in the way and
    /// how to go on.
    fn restart(&self, sessions: &mut Sessions) -> Option<Restart> {
        let State::Installed { version, binary } = &self.state else {
            return None;
        };
        match sessions.restart(binary, version) {
            Ok(restart) => Some(restart),
            Err(reason) => {
                let origin = &sessions.list[0].app;
                let resume = if origin.agent.state().messages.is_empty() {
                    String::new()
                } else {
                    format!(" --resume {}", origin.recorder.session_id())
                };
                sessions.active_mut().notice(format!(
                    "installed cupel {version}, but {reason} - run /update again when that \
                    is done, or quit and start cupel{resume}"
                ));
                None
            }
        }
    }
}

/// A spawned task outlives the handle that started it. Dropping `Updates` (cupel quits)
/// stops the task here.
impl Drop for Updates {
    fn drop(&mut self) {
        if let State::Checking(task) | State::Updating(task) = &self.state {
            task.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::Cell;
    use crate::ui::tests::{has_notice, run_command, test_app, test_app_with_home};

    fn notices(sessions: &Sessions) -> usize {
        let cells = &sessions.active().transcript.cells;
        cells
            .iter()
            .filter(|cell| matches!(cell, Cell::Notice { .. }))
            .count()
    }

    #[tokio::test]
    async fn a_newer_release_gets_one_notice() {
        let mut updates = Updates::default();
        updates.state = State::Checking(tokio::spawn(async {}));
        let mut sessions = Sessions::new(test_app());
        let newer = Version::new(99, 0, 0);
        assert!(
            updates
                .on_event(UpdateEvent::Checked(Some(newer)), &mut sessions)
                .is_none()
        );
        let running = update::current_version();
        let text = format!(
            "cupel 99.0.0 is available - you're running {running}. Run /update to install it."
        );
        assert!(has_notice(sessions.active(), &text));
        assert!(matches!(updates.state, State::Idle));
    }

    #[tokio::test]
    async fn an_up_to_date_check_says_nothing() {
        let mut updates = Updates::default();
        updates.state = State::Checking(tokio::spawn(async {}));
        let mut sessions = Sessions::new(test_app());
        let before = notices(&sessions);
        assert!(
            updates
                .on_event(UpdateEvent::Checked(None), &mut sessions)
                .is_none()
        );
        assert_eq!(notices(&sessions), before);
        assert!(matches!(updates.state, State::Idle));
    }

    #[tokio::test]
    async fn events_arrive_through_the_channel() {
        let mut updates = Updates::default();
        updates.sender.send(UpdateEvent::Checked(None)).unwrap();
        assert!(matches!(
            updates.next_event().await,
            UpdateEvent::Checked(None)
        ));
    }

    #[tokio::test]
    async fn dropping_updates_stops_the_check() {
        let mut updates = Updates::default();
        let task = tokio::spawn(std::future::pending::<()>());
        let handle = task.abort_handle();
        updates.state = State::Checking(task);
        drop(updates);
        // The abort takes effect the next time the runtime looks at the task.
        tokio::task::yield_now().await;
        assert!(handle.is_finished());
    }

    #[test]
    fn typing_update_asks_the_event_loop_and_never_the_model() {
        let mut app = test_app();
        run_command(&mut app, "/update");
        assert!(app.update_requested);
        assert!(app.pending_prompt.is_none());
        assert!(app.transcript.cells.is_empty());
    }

    #[tokio::test]
    async fn update_waits_until_every_session_is_idle() {
        let mut sessions = Sessions::new(test_app());
        let (events, _sink) = cupel_agent::agent_loop::agent_event_channel();
        sessions.active_mut().run_events = Some(events);
        let mut updates = Updates::default();
        assert!(updates.request(&mut sessions).is_none());
        let text = "origin still working - run /update when every session is idle";
        assert!(has_notice(sessions.active(), text));
        assert!(matches!(updates.state, State::Idle));
    }

    #[tokio::test]
    async fn one_update_at_a_time() {
        let mut sessions = Sessions::new(test_app());
        let mut updates = Updates::default();
        updates.state = State::Updating(tokio::spawn(std::future::pending()));
        assert!(updates.request(&mut sessions).is_none());
        assert!(has_notice(
            sessions.active(),
            "an update is running already"
        ));
    }

    #[test]
    fn an_install_restarts_unless_something_is_in_the_way() {
        let mut sessions = Sessions::new(test_app());
        sessions.active_mut().input.insert_str("half a thought");
        let mut updates = Updates::default();
        let installed = Ok(Outcome::Installed {
            version: Version::new(99, 0, 0),
            binary: "/tmp/new/cupel".into(),
        });
        let restart = updates.on_event(UpdateEvent::Done(installed), &mut sessions);
        assert!(restart.is_none());
        let text = "installed cupel 99.0.0, but origin has unsent text in its input box - \
            run /update again when that is done, or quit and start cupel";
        assert!(has_notice(sessions.active(), text));

        // The next /update tries the restart without downloading again.
        sessions.active_mut().input.submit();
        let restart = updates.request(&mut sessions).expect("nothing in the way");
        assert_eq!(restart.binary, std::path::Path::new("/tmp/new/cupel"));
        assert!(matches!(updates.state, State::Installed { .. }));
    }

    #[test]
    fn an_install_restarts_immediately_when_every_session_is_safe() {
        let mut sessions = Sessions::new(test_app());
        let mut updates = Updates::default();
        let installed = Ok(Outcome::Installed {
            version: Version::new(99, 0, 0),
            binary: "/tmp/new/cupel".into(),
        });
        let restart = updates
            .on_event(UpdateEvent::Done(installed), &mut sessions)
            .expect("a fresh session loses nothing");
        assert_eq!(restart.binary, std::path::Path::new("/tmp/new/cupel"));
        assert_eq!(restart.note, "cupel 99.0.0 is installed - restarting");
        assert!(matches!(updates.state, State::Installed { .. }));
        assert_eq!(notices(&sessions), 0);
    }

    #[test]
    fn a_blocked_restart_keeps_the_resume_id_for_the_next_attempt() {
        let root = std::env::temp_dir().join(format!("cupel-update-resume-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mut app = test_app_with_home(&root, "cupel-resume");
        let message = cupel_agent::AgentMessage::user_text("keep this conversation");
        app.recorder.record(&message);
        let mut options =
            cupel_agent::AgentOptions::new(app.agent.state().model, app.agent.registry());
        options.messages = vec![message];
        let app = crate::app::App::new(cupel_agent::Agent::new(options), app.meta, app.recorder);
        let mut sessions = Sessions::new(app);
        sessions.active_mut().input.insert_str("still typing");
        let mut updates = Updates::default();
        let installed = Ok(Outcome::Installed {
            version: Version::new(99, 0, 0),
            binary: "/tmp/new/cupel".into(),
        });
        assert!(
            updates
                .on_event(UpdateEvent::Done(installed), &mut sessions)
                .is_none()
        );
        assert!(has_notice(
            sessions.active(),
            "quit and start cupel --resume cupel-resume"
        ));
        sessions.active_mut().input.submit();
        let restart = updates.request(&mut sessions).expect("no second download");
        assert!(
            restart
                .args
                .ends_with(&["--resume".to_string(), "cupel-resume".to_string()])
        );
        assert!(matches!(updates.state, State::Installed { .. }));
    }

    #[tokio::test]
    async fn update_reports_its_progress_and_how_it_ended() {
        let mut sessions = Sessions::new(test_app());
        let mut updates = Updates::default();
        let running = update::current_version();

        let progress = UpdateEvent::Progress("downloading cupel 99.0.0...".to_string());
        assert!(updates.on_event(progress, &mut sessions).is_none());
        assert!(has_notice(sessions.active(), "downloading cupel 99.0.0..."));

        let newest = Ok(Outcome::UpToDate(running.clone()));
        assert!(
            updates
                .on_event(UpdateEvent::Done(newest), &mut sessions)
                .is_none()
        );
        let text = format!(
            "nothing to install - you're running {running}, the newest release is {running}"
        );
        assert!(has_notice(sessions.active(), &text));

        let failed = Err(UpdateError::Network("offline".to_string()));
        assert!(
            updates
                .on_event(UpdateEvent::Done(failed), &mut sessions)
                .is_none()
        );
        let text = "update failed: cannot reach GitHub: offline";
        assert!(has_notice(sessions.active(), text));
        assert!(matches!(updates.state, State::Idle));
    }

    #[tokio::test]
    async fn a_check_answer_after_update_started_is_dropped() {
        let mut sessions = Sessions::new(test_app());
        let mut updates = Updates::default();
        updates.state = State::Updating(tokio::spawn(std::future::pending()));
        let before = notices(&sessions);
        let newer = UpdateEvent::Checked(Some(Version::new(99, 0, 0)));
        assert!(updates.on_event(newer, &mut sessions).is_none());
        assert_eq!(notices(&sessions), before);
        assert!(matches!(updates.state, State::Updating(_)));
    }

    #[tokio::test]
    async fn update_runs_in_the_background_and_reports_back() {
        // A port that was free a moment ago: GitHub is "offline".
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        drop(listener);
        let mut sessions = Sessions::new(test_app());
        let mut updates = Updates::default();
        updates.url = format!("http://{address}/latest");
        assert!(updates.request(&mut sessions).is_none());
        assert!(has_notice(
            sessions.active(),
            "looking for a newer release..."
        ));
        assert!(matches!(updates.state, State::Updating(_)));
        loop {
            let event = updates.next_event().await;
            let done = matches!(event, UpdateEvent::Done(_));
            assert!(updates.on_event(event, &mut sessions).is_none());
            if done {
                break;
            }
        }
        let text = "update failed: cannot reach GitHub";
        assert!(has_notice(sessions.active(), text));
        assert!(matches!(updates.state, State::Idle));
    }
}
