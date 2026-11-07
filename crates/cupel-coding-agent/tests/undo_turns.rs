//! Real agent-loop/tool/recorder integration with a scripted, offline provider.
#![allow(clippy::tests_outside_test_module)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use cupel_agent::{Agent, AgentEvent, AgentMessage, AgentOptions};
use cupel_coding_agent::bootstrap::{self, Frontend};
use cupel_coding_agent::session::SessionRecorder;
use cupel_coding_agent::undo::{self, UndoCommand};
use cupel_core::event_stream::{AssistantMessageStream, assistant_message_channel};
use cupel_core::provider::Provider;
use cupel_core::types::{
    Api, AssistantContent, AssistantMessage, Context, Model, StopReason, StreamOptions,
    TextContent, ToolCall, Usage, now_ms,
};
use futures_util::StreamExt as _;
use tokio::sync::Notify;

struct Script {
    calls: AtomicUsize,
    steering: bool,
    ready: Arc<Notify>,
}

impl Provider for Script {
    fn api(&self) -> &str {
        "undo-test"
    }

    fn stream(
        &self,
        model: &Model,
        _context: Context,
        options: StreamOptions,
    ) -> AssistantMessageStream {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        let edit = match call {
            0 => Some(("mine", "intermediate")),
            1 => Some(("intermediate", "first-end")),
            3 => Some(("first-end", "second-end")),
            _ => None,
        };
        let content = edit.map_or_else(
            || vec![AssistantContent::Text(TextContent::plain("done"))],
            |(before, after)| vec![AssistantContent::ToolCall(ToolCall {
                id: format!("call-{call}"),
                name: "apply_patch".to_string(),
                arguments: serde_json::json!({"input": format!("*** Begin Patch\n*** Update File: a\n@@\n-{before}\n+{after}\n*** End Patch")}),
            })],
        );
        let mut message = AssistantMessage {
            content,
            api: model.api.clone(),
            provider: model.provider.clone(),
            model: model.id.clone(),
            response_model: None,
            response_id: None,
            usage: Usage::default(),
            stop_reason: if edit.is_some() {
                StopReason::ToolUse
            } else {
                StopReason::Stop
            },
            error_message: None,
            error_kind: None,
            timestamp: now_ms(),
        };
        let (stream, sink) = assistant_message_channel();
        let _ = sink.start();
        if self.steering && call == 2 {
            self.ready.notify_one();
            tokio::spawn(async move {
                options.signal.unwrap().cancelled().await;
                message.stop_reason = StopReason::Aborted;
                let _ = sink.error(StopReason::Aborted, message);
            });
        } else {
            let _ = sink.done(message.stop_reason, message);
        }
        stream
    }
}

struct Project(PathBuf);

impl Project {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "cupel-undo-integration-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(path.join("project")).unwrap();
        Self(path.canonicalize().unwrap())
    }

    fn cwd(&self) -> PathBuf {
        self.0.join("project")
    }
    fn home(&self) -> PathBuf {
        self.0.join("home")
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

async fn build(
    project: &Project,
    registry: Arc<cupel_core::provider::Registry>,
    model: &Model,
    messages: Vec<AgentMessage>,
) -> (Agent, SessionRecorder) {
    let cwd = project.cwd();
    let recorder = SessionRecorder::new(Some(project.home()), &cwd, "undo-session", &model.id);
    let mut ingredients =
        bootstrap::load(&cwd, Some(project.home()), &registry, Frontend::Plain).await;
    ingredients.bind_session(&recorder, &cwd);
    let mut options = AgentOptions::new(model.clone(), registry);
    options.system_prompt = ingredients.system_prompt;
    options.tools = ingredients.tools;
    options.hooks = Arc::new(ingredients.hooks);
    options.api_key = Some("test".to_string());
    options.messages = messages;
    (Agent::new(options), recorder)
}

async fn exercise(steering: bool) {
    let project = Project::new(if steering { "steering" } else { "follow-up" });
    std::fs::write(project.cwd().join("a"), "mine\n").unwrap();
    let ready = Arc::new(Notify::new());
    let mut registry = cupel_core::default_registry();
    registry.register(Arc::new(Script {
        calls: AtomicUsize::new(0),
        steering,
        ready: Arc::clone(&ready),
    }));
    let registry = Arc::new(registry);
    let mut model = cupel_core::catalog::builtin_models().remove(0);
    model.api = Api::from("undo-test");
    let (mut agent, mut recorder) =
        build(&project, Arc::clone(&registry), &model, Vec::new()).await;
    if !steering {
        agent.follow_up(AgentMessage::user_text("second assignment"));
    }
    recorder.before_prompt("first assignment").await;
    let mut events = agent.prompt_text("first assignment").unwrap();
    if steering {
        tokio::time::timeout(std::time::Duration::from_secs(10), ready.notified())
            .await
            .unwrap();
        agent.steer(AgentMessage::user_text("second assignment"));
    }
    // Deliberately don't consume frontend events until all tools have finished.
    // Turn grouping must happen in the loop, not in a lagging UI event handler.
    tokio::time::timeout(std::time::Duration::from_secs(10), agent.wait_for_idle())
        .await
        .unwrap();
    while let Some(event) = events.next().await {
        match event {
            AgentEvent::MessageEnd { message } => recorder.record(&message),
            AgentEvent::AgentEnd { .. } => recorder.on_agent_end(),
            AgentEvent::ToolExecutionEnd {
                is_error, result, ..
            } => assert!(!is_error, "{result:?}"),
            _ => {}
        }
    }
    assert_eq!(
        std::fs::read(project.cwd().join("a")).unwrap(),
        b"second-end\n",
        "both assignments must finish before frontend event consumption"
    );
    let transcript = recorder.transcript().unwrap().to_path_buf();
    // An interrupted session retains crash-recovery snapshots. An orderly
    // end_session instead removes them, even when the transcript is resumed.
    drop(agent);
    drop(recorder);

    let (_, messages) = cupel_coding_agent::session::load_transcript(&transcript).unwrap();
    let (mut agent, mut recorder) = build(&project, registry, &model, messages).await;
    undo::execute_command(UndoCommand::Undo, &mut agent, &mut recorder)
        .await
        .unwrap();
    assert_eq!(
        std::fs::read(project.cwd().join("a")).unwrap(),
        b"first-end\n",
        "undo must remove only the follow-up or steering assignment"
    );
    undo::execute_command(UndoCommand::Undo, &mut agent, &mut recorder)
        .await
        .unwrap();
    assert_eq!(
        std::fs::read(project.cwd().join("a")).unwrap(),
        b"mine\n",
        "the first assignment's two patches must undo together"
    );
    let (_, messages) = cupel_coding_agent::session::load_transcript(&transcript).unwrap();
    assert_eq!(
        messages,
        agent.state().messages,
        "undo notices must survive the next resume"
    );
    assert!(
        Path::new(&transcript)
            .with_extension("")
            .join("undo")
            .is_dir(),
        "snapshots belong beside the resumed transcript, outside the checkout"
    );
    recorder.end_session().await;
    assert!(
        !transcript.with_extension("").join("undo").exists(),
        "normal session closure must reclaim even already-undone snapshots"
    );
    assert!(transcript.exists(), "session history must remain resumable");
}

#[tokio::test]
async fn queued_prompts_have_separate_undo_turns_even_when_frontend_events_lag() {
    exercise(false).await;
}

#[tokio::test]
async fn consumed_steering_starts_a_new_undo_turn_after_running_tools_settle() {
    exercise(true).await;
}
