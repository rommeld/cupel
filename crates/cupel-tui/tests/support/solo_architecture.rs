//! Candidate adapter only; the shared observation fixture is identical in both trees.
use std::sync::Arc;

use cupel_agent::AgentMessage;
use cupel_coding_agent::commands::TeamCommand;
use cupel_coding_agent::team_conversation::TeamRuntime;
use cupel_tui::app::{App, AppEvent};
use cupel_tui::startup::StartupCommand;
use serde_json::{Value, json};

pub fn activate(app: &mut App) {
    app.activate_collaboration();
}

pub fn runtime(app: &App) -> &Arc<TeamRuntime> {
    &app.collaboration.as_ref().unwrap().runtime
}

pub async fn settle(app: &mut App) {
    let entries = runtime(app).inspect(None).await.unwrap().entries;
    for entry in entries {
        app.on_event(AppEvent::Team(Some(
            cupel_coding_agent::team_conversation::TeamUpdate::Entry(entry),
        )))
        .await;
    }
}

pub async fn clear(app: &mut App) {
    app.apply_startup(StartupCommand::Team(TeamCommand::Clear));
    app.finish_mode_change().await;
}

pub fn mode(app: &App) -> Value {
    json!({"collaboration": app.collaboration.is_some(),
        "base_hooks": Arc::ptr_eq(&app.agent.hooks(), &app.base_hooks),
        "base_tools": app.agent.tools().len() == app.base_tools.len()
            && app.agent.tools().iter().zip(&app.base_tools).all(|(actual, base)| Arc::ptr_eq(actual, base)),
    })
}

pub async fn archive(app: &mut App) -> Value {
    let entries = if let Some(active) = &app.collaboration {
        active.runtime.inspect(None).await.unwrap().entries
    } else {
        let messages = app.agent.state().messages;
        let saved =
            messages
                .iter()
                .rev()
                .find_map(|message| match message {
                    AgentMessage::Custom { kind, payload, .. }
                    | AgentMessage::Metadata { kind, payload, .. }
                        if kind == "cupel.team-transition" =>
                    {
                        Some(
                            serde_json::from_value::<
                                cupel_coding_agent::team_conversation::TeamArchive,
                            >(payload["archive"].clone())
                            .unwrap(),
                        )
                    }
                    _ => None,
                })
                .expect("cleared team must preserve an archive reference");
        if let Some(path) = &saved.path {
            let loaded = cupel_coding_agent::team_conversation::TeamArchive::load(path).unwrap();
            assert_eq!(
                loaded.entries, saved.entries,
                "readable archive and exact snapshot diverged"
            );
            let readable = std::fs::read_to_string(path).unwrap();
            assert!(
                readable.contains("SHARED_POST_MARKER")
                    && readable.contains("SHARED_RESULT_MARKER"),
                "ordinary read tool archive must preserve both fixture facts"
            );
        }
        // A read traverses the real lazy UI path, which must remain in solo mode.
        app.read_team_conversation(None).await;
        assert!(
            app.collaboration.is_none(),
            "reading history must not reactivate teamwork"
        );
        saved.entries
    };
    serde_json::to_value(entries).unwrap()
}

pub fn assert_candidate(observation: &Value) {
    let active = observation["case"] == "team";
    assert_eq!(
        observation["mode_before"]["collaboration"], active,
        "wrong collaboration state before run"
    );
    assert_eq!(
        observation["mode_after"]["collaboration"], active,
        "wrong collaboration state after render/run/abort"
    );
    if !active {
        for mode in ["mode_before", "mode_after"] {
            assert_eq!(
                observation[mode]["base_hooks"], true,
                "solo must retain the supplied hook Arc"
            );
            assert_eq!(
                observation[mode]["base_tools"], true,
                "solo must retain every supplied tool Arc"
            );
        }
    }
}

/// Mirror only the documented crash-safe handoff reconstruction at the transcript boundary.
pub fn rehydrate(messages: &[AgentMessage]) -> Vec<AgentMessage> {
    let latest = messages
        .iter()
        .enumerate()
        .rev()
        .find_map(|(index, message)| match message {
            AgentMessage::Custom {
                kind,
                payload,
                timestamp,
            }
            | AgentMessage::Metadata {
                kind,
                payload,
                timestamp,
                ..
            } if kind == "cupel.team-transition" => payload["handoff"]
                .as_str()
                .filter(|text| !text.is_empty())
                .map(|text| (index, text, *timestamp)),
            _ => None,
        });
    let mut result = messages.to_vec();
    if let Some((index, handoff, timestamp)) = latest {
        let already_present = matches!(messages.get(index + 1),
            Some(AgentMessage::Llm(cupel_core::types::Message::User(user)))
                if matches!(&user.content, cupel_core::types::UserContentBody::Text(text) if text == handoff));
        if !already_present {
            result.insert(
                index + 1,
                AgentMessage::Llm(cupel_core::types::Message::User(
                    cupel_core::types::UserMessage {
                        content: cupel_core::types::UserContentBody::Text(handoff.into()),
                        timestamp,
                    },
                )),
            );
        }
    }
    result
}
