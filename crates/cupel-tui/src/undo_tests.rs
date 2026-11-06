use std::path::PathBuf;

use cupel_agent::types::AgentTool as _;
use cupel_coding_agent::session::SessionRecorder;
use cupel_coding_agent::tools::apply_patch::ApplyPatchTool;
use cupel_coding_agent::undo::UndoCommand;
use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use tokio_util::sync::CancellationToken;

use crate::app::{App, ReloadTarget};
use crate::transcript::Cell;
use crate::ui::tests::test_app;

fn command(app: &mut App, text: &str) {
    app.input.insert_str(text);
    app.on_terminal_event(Event::Key(KeyEvent::new(
        KeyCode::Enter,
        KeyModifiers::NONE,
    )));
}

fn notices(app: &App) -> String {
    app.transcript
        .cells
        .iter()
        .filter_map(|cell| match cell {
            Cell::Notice { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

struct Project(PathBuf);

impl Project {
    fn new(name: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("cupel-ui-undo-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("project")).unwrap();
        Self(root.canonicalize().unwrap())
    }

    fn app(&self) -> App {
        let mut app = test_app();
        let cwd = self.0.join("project");
        app.meta.cwd = cwd.display().to_string();
        app.meta.home = Some(self.0.join("home"));
        app.recorder = SessionRecorder::new(app.meta.home.clone(), &cwd, "undo-ui", "test-model");
        app
    }

    async fn edit(&self, app: &mut App) {
        let cwd = self.0.join("project");
        std::fs::write(cwd.join("a"), "mine\n").unwrap();
        app.recorder.before_prompt("edit a").await;
        let message = cupel_agent::AgentMessage::user_text("edit a");
        app.recorder.record(&message);
        app.agent.push_message(message);
        ApplyPatchTool::new(&cwd).with_undo(app.recorder.undo())
            .execute("patch", serde_json::json!({"input": "*** Begin Patch\n*** Update File: a\n@@\n-mine\n+agent\n*** End Patch"}), CancellationToken::new(), None).await.unwrap();
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
async fn undo_commands_dispatch_locally_and_survive_hot_reload_and_resume() {
    let project = Project::new("reload");
    let mut app = project.app();
    project.edit(&mut app).await;
    app = app.hot_reload(ReloadTarget::Current).await;
    command(&mut app, "/changes");
    assert_eq!(app.pending_undo, Some(UndoCommand::Changes));
    assert!(app.pending_prompt.is_none());
    let pending = app.pending_undo.take().unwrap();
    app.run_undo_command(pending).await;
    assert!(notices(&app).contains("-mine\n+agent"));
    app = app
        .hot_reload(ReloadTarget::Resume("edit a".to_string()))
        .await;
    command(&mut app, "/undo");
    let pending = app.pending_undo.take().unwrap();
    app.run_undo_command(pending).await;
    assert!(notices(&app).contains("Undid patch edits"));
    assert_eq!(
        std::fs::read(project.0.join("project/a")).unwrap(),
        b"mine\n"
    );
}

#[tokio::test]
async fn new_sessions_never_undo_another_sessions_edits() {
    let project = Project::new("new");
    let mut app = project.app();
    project.edit(&mut app).await;
    app = app.hot_reload(ReloadTarget::New).await;
    app.run_undo_command(UndoCommand::Undo).await;
    assert!(notices(&app).contains("No recorded patch edits"));
    assert_eq!(
        std::fs::read(project.0.join("project/a")).unwrap(),
        b"agent\n"
    );
}

#[tokio::test]
async fn undo_refuses_accepted_work_arguments_and_busy_checkout_gates() {
    let project = Project::new("busy");
    let mut app = project.app();
    project.edit(&mut app).await;
    command(&mut app, "/undo force");
    assert!(app.pending_undo.is_none());
    assert!(notices(&app).contains("no arguments"));
    app.pending_prompt = Some("accepted".to_string());
    command(&mut app, "/undo");
    assert!(app.pending_undo.is_none());
    assert!(notices(&app).contains("agent is working"));
    app.pending_prompt = None;
    let gate = std::sync::Arc::clone(&app.write_gate);
    let _guard = gate.lock().await;
    app.run_undo_command(UndoCommand::Undo).await;
    assert!(notices(&app).contains("checkout is being modified"));
    assert_eq!(
        std::fs::read(project.0.join("project/a")).unwrap(),
        b"agent\n"
    );
}
