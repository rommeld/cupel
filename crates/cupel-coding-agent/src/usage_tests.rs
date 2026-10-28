use std::path::{Path, PathBuf};

use cupel_agent::AgentMessage;
use cupel_core::types::{Api, AssistantMessage, Cost, Message, Provider, StopReason, Usage};

use crate::session::TranscriptHeader;
use crate::usage::{UsageByModel, load_history};

struct TempHome(PathBuf);

impl TempHome {
    fn new() -> Self {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("cupel-usage-{}-{nonce}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }
}

impl Drop for TempHome {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn reply(provider: &str, model: &str, input: u64) -> AssistantMessage {
    AssistantMessage {
        content: Vec::new(),
        api: Api::from("test"),
        provider: Provider::from(provider),
        model: model.to_string(),
        response_model: Some("served-version-do-not-group-by-this".into()),
        response_id: None,
        usage: Usage {
            input,
            output: 4,
            cache_read: 6,
            cache_write: 7,
            reasoning: Some(3),
            cost: Cost {
                total: 0.25,
                ..Cost::default()
            },
            ..Usage::default()
        },
        stop_reason: StopReason::Stop,
        error_message: None,
        error_kind: None,
        timestamp: 0,
    }
}

fn transcript(home: &Path, relative: &str, version: u32, input: u64) -> PathBuf {
    let path = home.join("sessions").join(relative);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let header = TranscriptHeader {
        version,
        session_id: relative.to_string(),
        cwd: "/example".into(),
        model: "header-is-not-attribution".into(),
        started_at: 0,
    };
    let user = AgentMessage::user_text("hello");
    let assistant = AgentMessage::Llm(Message::Assistant(reply("p", "m", input)));
    std::fs::write(
        &path,
        format!(
            "{}\n{}\n{}\n",
            serde_json::to_string(&header).unwrap(),
            serde_json::to_string(&user).unwrap(),
            serde_json::to_string(&assistant).unwrap(),
        ),
    )
    .unwrap();
    path
}

#[test]
fn usage_groups_requested_models_by_provider_without_double_counting_reasoning() {
    let mut usage = UsageByModel::default();
    usage.record(&reply("a", "same", 10));
    usage.record(&reply("b", "same", 20));
    usage.record(&reply("a", "other", 30));
    usage.record(&reply("a", "same", 40));
    assert_eq!(usage.models.len(), 3);
    let totals = usage.totals();
    assert_eq!(
        (totals.input, totals.output, totals.cache_read),
        (100, 16, 24)
    );
    assert!((totals.cost - 1.0).abs() < f64::EPSILON);
    assert!(
        usage
            .models
            .keys()
            .all(|key| key.model != "served-version-do-not-group-by-this")
    );
    let labels: Vec<_> = usage
        .models
        .keys()
        .map(|key| (key.provider.as_str(), key.model.as_str()))
        .collect();
    assert_eq!(labels, [("a", "other"), ("a", "same"), ("b", "same")]);
}

#[test]
fn usage_keeps_reported_tokens_from_errors_and_aborts() {
    let mut usage = UsageByModel::default();
    for reason in [StopReason::Error, StopReason::Aborted, StopReason::Length] {
        let mut assistant = reply("p", "m", 10);
        assistant.stop_reason = reason;
        usage.record(&assistant);
    }
    assert_eq!(usage.totals().input, 30);
}

#[test]
fn usage_history_covers_projects_and_archives_but_excludes_the_current_file() {
    let home = TempHome::new();
    let current = transcript(&home.0, "project-a/current.jsonl", 1, 1000);
    transcript(&home.0, "project-a/older.jsonl", 1, 10);
    transcript(&home.0, "project-b/other.jsonl", 1, 20);
    transcript(&home.0, "project-a/archive/merged.jsonl", 1, 30);
    std::fs::write(
        home.0.join("sessions/project-a/not-a-transcript.txt"),
        "not JSON",
    )
    .unwrap();
    let history = load_history(Some(&home.0), Some(&current));
    assert_eq!(history.sessions, 3);
    assert_eq!(history.usage.totals().input, 60);
    assert!(history.warnings.is_empty());
    let mut all = history.usage;
    let mut current_snapshot = UsageByModel::default();
    current_snapshot.record(&reply("p", "m", 1100));
    all.merge(&current_snapshot);
    assert_eq!(
        all.totals().input,
        1160,
        "memory replaces disk, it is not added to it"
    );
}

#[test]
fn usage_history_reports_bad_headers_versions_and_partial_lines() {
    let home = TempHome::new();
    let partial = transcript(&home.0, "p/partial.jsonl", 1, 10);
    use std::io::Write as _;
    writeln!(
        std::fs::OpenOptions::new()
            .append(true)
            .open(partial)
            .unwrap(),
        "{{truncated"
    )
    .unwrap();
    transcript(&home.0, "p/future.jsonl", 99, 1000);
    std::fs::write(home.0.join("sessions/p/broken.jsonl"), "bad header\n").unwrap();
    let history = load_history(Some(&home.0), None);
    assert_eq!(history.sessions, 1);
    assert_eq!(history.usage.totals().input, 10);
    assert_eq!(history.warnings.len(), 3);
}

#[test]
fn usage_history_handles_no_home_and_a_missing_sessions_directory_without_writing() {
    let home = TempHome::new();
    let empty = load_history(Some(&home.0), None);
    assert_eq!(empty.sessions, 0);
    assert!(empty.warnings.is_empty());
    assert!(!home.0.join("sessions").exists());
    assert_eq!(load_history(None, None).warnings.len(), 1);
}

#[test]
fn team_journals_are_metadata_not_usage_transcripts() {
    let home = TempHome::new();
    transcript(&home.0, "p/session.jsonl", 1, 10);
    transcript(&home.0, "p/session/helper.jsonl", 1, 20);
    let journal = home.0.join("sessions/p/session/team");
    std::fs::create_dir_all(&journal).unwrap();
    std::fs::write(
        journal.join("conversation.jsonl"),
        "{\"schemaVersion\":1}\n",
    )
    .unwrap();
    std::fs::write(
        journal.join("conversation-00000001.jsonl"),
        "{\"schemaVersion\":1}\n",
    )
    .unwrap();
    let history = load_history(Some(&home.0), None);
    assert_eq!(history.usage.totals().input, 30);
    assert!(history.warnings.is_empty());
}

#[cfg(unix)]
#[test]
fn usage_history_does_not_follow_symlinks_or_count_linked_transcripts_twice() {
    let home = TempHome::new();
    let path = transcript(&home.0, "p/one.jsonl", 1, 10);
    std::os::unix::fs::symlink(path, home.0.join("sessions/p/link.jsonl")).unwrap();
    std::os::unix::fs::symlink(home.0.join("sessions"), home.0.join("sessions/p/loop")).unwrap();
    let history = load_history(Some(&home.0), None);
    assert_eq!(history.sessions, 1);
    assert_eq!(history.usage.totals().input, 10);
}
