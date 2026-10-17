//! Updates in the background: the check at startup.
//!
//! The work runs in a spawned task that reports over a channel, the way a login does
//! (login.rs). Unlike a login it belongs to the whole process, not to one session: the
//! event loop owns the one [`Updates`], so `/new`, `/hot-reload` and switching sessions
//! never start a second check, and its notice comes once.

use cupel_coding_agent::update::{self, Version};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::sessions::Sessions;

/// What an update task reports.
pub enum UpdateEvent {
    Checked(Option<Version>),
}

/// What the update work does right now. The states exclude each other, so they are one
/// enum and not a set of flags that could contradict each other.
enum State {
    Idle,
    Checking(JoinHandle<()>),
}

/// The update work of this process, owned by the event loop.
pub struct Updates {
    sender: mpsc::UnboundedSender<UpdateEvent>,
    events: mpsc::UnboundedReceiver<UpdateEvent>,
    state: State,
}

impl Default for Updates {
    fn default() -> Self {
        let (sender, events) = mpsc::unbounded_channel();
        Self {
            sender,
            events,
            state: State::Idle,
        }
    }
}

impl Updates {
    /// Ask GitHub in the background whether a newer release is out. A failure (offline,
    /// a timeout, the rate limit) only reaches the log: the ckeck is a courtesy and
    /// must never get in the way.
    pub fn check(&mut self) {
        let sender = self.sender.clone();
        let task = tokio::spawn(async move {
            let current = update::current_version();
            let url = update::LATEST_RELEASE_URL;
            let newer = match update::check(&update::client(), url, &current).await {
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

    /// The next event of a task. While there is none, this waits.
    pub async fn next_event(&mut self) -> UpdateEvent {
        match self.events.recv().await {
            Some(event) => event,
            None => std::future::pending().await,
        }
    }

    /// Show what a task reported, in the session on screen.
    pub fn on_event(&mut self, event: UpdateEvent, sessions: &mut Sessions) {
        match event {
            UpdateEvent::Checked(newer) => {
                self.state = State::Idle;
                if let Some(version) = newer {
                    sessions.active_mut().notice(format!(
                        "cupel {version} is available - you're running {}",
                        update::current_version(),
                    ));
                }
            }
        }
    }
}

/// A spawned task outlives the handle that started it. Dropping `Updates` (cupel quits)
/// stops the task here.
impl Drop for Updates {
    fn drop(&mut self) {
        if let State::Checking(task) = &self.state {
            task.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::Cell;
    use crate::ui::tests::{has_notice, test_app};

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
        updates.on_event(UpdateEvent::Checked(Some(newer)), &mut sessions);
        let running = update::current_version();
        let text = format!("cupel 99.0.0 is available - you're running {running}");
        assert!(has_notice(sessions.active(), &text));
        assert!(matches!(updates.state, State::Idle));
    }

    #[tokio::test]
    async fn an_up_to_date_check_says_nothing() {
        let mut updates = Updates::default();
        updates.state = State::Checking(tokio::spawn(async {}));
        let mut sessions = Sessions::new(test_app());
        let before = notices(&sessions);
        updates.on_event(UpdateEvent::Checked(None), &mut sessions);
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
}
