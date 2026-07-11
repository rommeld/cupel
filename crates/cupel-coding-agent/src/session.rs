//! Session persistence: every conversation is recorded as a JSONL
//! transcript so it can be resumed (`cupel --resume`) and consumed by
//! external tools (e.g. the `entire` CLI's agent protocol).
//!
//! Layout: `<cupel home>/sessions/<project-slug>/<session-id>.jsonl`, where
//! the slug is the cwd with every non-alphanumeric character mapped to `-`
//! (the same scheme Claude Code uses for `~/.claude/projects/`). Line 1 is
//! a header object; every following line is one [`AgentMessage`] in its
//! existing serde form. Appends are flushed per message, so a crash loses
//! at most the in-flight message.
//!
//! Like `.cupel/` scaffolding, the transcript is created LAZILY on the
//! first agent interaction - launching and quitting cupel leaves no trace.
//! Persisted history is never rewritten by compaction (compaction only
//! mutates a run's private context snapshot), so a transcript is always the
//! full conversation; resume replays it and the loop re-compacts as needed.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use cupel_agent::AgentMessage;
use serde::{Deserialize, Serialize};

use crate::hooks::{HookEvent, HookRunner};

/// Bumped when the transcript format changes incompatibly; the loader
/// refuses to resume a version it doesn't understand.
pub const TRANSCRIPT_VERSION: u32 = 1;

/// Line 1 of every transcript: session facts AT START. Deliberately not
/// updated afterwards (a mid-session `/model` switch is visible in the
/// assistant messages themselves).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TranscriptHeader {
    pub version: u32,
    pub session_id: String,
    pub cwd: String,
    pub model: String,
    pub started_at: u64,
}

/// The cwd as a filesystem-safe directory name: every char outside
/// `[A-Za-z0-9_-]` becomes `-`. `/a/b` and `/a-b` can collide - acceptable
/// for a per-user convenience layout, and the header records the real cwd.
#[must_use]
pub fn project_slug(cwd: &Path) -> String {
    cwd.display()
        .to_string()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// `<home>/sessions/<slug>` - `None` (no resolvable home) disables
/// persistence entirely.
#[must_use]
pub fn sessions_dir(home: Option<&Path>, cwd: &Path) -> Option<PathBuf> {
    Some(home?.join("sessions").join(project_slug(cwd)))
}

/// The newest transcript (by modification time) for this project - what a
/// bare `--resume` picks.
#[must_use]
pub fn find_latest(home: Option<&Path>, cwd: &Path) -> Option<PathBuf> {
    let dir = sessions_dir(home, cwd)?;
    let entries = std::fs::read_dir(dir).ok()?;
    entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "jsonl"))
        .max_by_key(|p| {
            std::fs::metadata(p)
                .and_then(|m| m.modified())
                .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
        })
}

/// Parse a transcript: header off line 1, one message per following line.
/// Malformed MESSAGE lines are skipped with a warning (a crash mid-append
/// can truncate the last line); a malformed or wrong-version HEADER is an
/// error, because resuming without trusted session facts would be a guess.
pub fn load_transcript(path: &Path) -> Result<(TranscriptHeader, Vec<AgentMessage>), String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read transcript {}: {e}", path.display()))?;
    let mut lines = content.lines();

    let header_line = lines.next().ok_or("transcript is empty")?;
    let header: TranscriptHeader = serde_json::from_str(header_line)
        .map_err(|e| format!("transcript header is not readable: {e}"))?;
    if header.version != TRANSCRIPT_VERSION {
        return Err(format!(
            "transcript version {} is not supported (this cupel reads version {TRANSCRIPT_VERSION})",
            header.version
        ));
    }

    let mut messages = Vec::new();
    for (i, line) in lines.enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<AgentMessage>(line) {
            Ok(message) => messages.push(message),
            // +2: 1-based and the header consumed line 1.
            Err(e) => tracing::warn!(line = i + 2, "skipping malformed transcript line: {e}"),
        }
    }
    Ok((header, messages))
}

/// Owns one session's transcript file and hook dispatch. Constructed
/// unconditionally by `main` and threaded into the frontends; with no
/// resolvable cupel home it degrades to a no-op (hooks still run - they
/// only need directories, not the home).
pub struct SessionRecorder {
    header: TranscriptHeader,
    /// Full transcript path; `None` disables persistence.
    path: Option<PathBuf>,
    /// Opened lazily on the first write (no trace before interaction).
    file: Option<std::fs::File>,
    hooks: HookRunner,
    /// Whether `session-start` has fired in this process.
    started: bool,
}

impl SessionRecorder {
    /// Pure constructor - touches no filesystem. `home` should be
    /// [`crate::resources::config_home`].
    #[must_use]
    pub fn new(home: Option<PathBuf>, cwd: &Path, session_id: &str, model_id: &str) -> Self {
        let path =
            sessions_dir(home.as_deref(), cwd).map(|d| d.join(format!("{session_id}.jsonl")));
        // Hook roots mirror the resource roots (home, then project .cupel),
        // minus the raw cwd - hooks/ at the repo root would be clutter.
        let mut hook_roots = Vec::new();
        if let Some(home) = &home {
            hook_roots.push(home.clone());
        }
        hook_roots.push(cwd.join(".cupel"));
        let session_ref = path.clone().unwrap_or_default();
        Self {
            header: TranscriptHeader {
                version: TRANSCRIPT_VERSION,
                session_id: session_id.to_string(),
                cwd: cwd.display().to_string(),
                model: model_id.to_string(),
                started_at: cupel_core::types::now_ms(),
            },
            path,
            file: None,
            hooks: HookRunner::new(hook_roots, session_id, &session_ref, cwd),
            started: false,
        }
    }

    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.header.session_id
    }

    /// A prompt is about to start a run. Settles any pending `stop` hook
    /// first (ordering guarantee for external consumers), lazily creates
    /// the transcript, then fires `session-start` (once) and
    /// `user-prompt-submit` - both awaited, bounded by the hook timeout.
    pub async fn before_prompt(&mut self, prompt: &str) {
        self.ensure_file();
        if !self.started {
            self.started = true;
            self.hooks.dispatch(HookEvent::SessionStart).await;
        }
        self.hooks
            .dispatch(HookEvent::UserPromptSubmit { prompt })
            .await;
    }

    /// A steering prompt was queued mid-run: fire its hook in the
    /// background (the TUI key handler must not block on hook processes).
    pub fn on_steer(&mut self, prompt: &str) {
        self.hooks
            .fire_background(HookEvent::UserPromptSubmit { prompt });
    }

    /// Append one finalized message (the `MessageEnd` tap) and flush, so a
    /// crash loses at most the message currently streaming.
    pub fn record(&mut self, message: &AgentMessage) {
        self.ensure_file();
        let Some(file) = &mut self.file else {
            return;
        };
        let write = serde_json::to_string(message)
            .map_err(|e| e.to_string())
            .and_then(|line| {
                writeln!(file, "{line}")
                    .and_then(|()| file.flush())
                    .map_err(|e| e.to_string())
            });
        if let Err(e) = write {
            tracing::warn!("failed to append transcript message: {e}");
        }
    }

    /// The agent run finished: fire `stop` without blocking the frontend.
    pub fn on_agent_end(&mut self) {
        self.hooks.fire_background(HookEvent::Stop);
    }

    /// cupel is exiting normally: drain the hook chain, then `session-end`.
    /// (A killed process skips this - the transcript is still intact thanks
    /// to per-message flushing.)
    pub async fn end_session(&mut self) {
        // Only a session that actually started (had a prompt) announces an
        // end - launch+quit stays completely silent.
        if self.started {
            self.hooks.dispatch(HookEvent::SessionEnd).await;
        }
    }

    /// Open the transcript for appending, writing the header if (and only
    /// if) the file is new - on resume the original header is kept. Every
    /// failure disables persistence for the session with one warning.
    fn ensure_file(&mut self) {
        if self.file.is_some() {
            return;
        }
        let Some(path) = &self.path else {
            tracing::debug!("no cupel home resolvable - session persistence disabled");
            return;
        };
        let open = || -> std::io::Result<std::fs::File> {
            if let Some(dir) = path.parent() {
                std::fs::create_dir_all(dir)?;
            }
            let is_new = !path.exists();
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)?;
            if is_new {
                let header = serde_json::to_string(&self.header)
                    .map_err(|e| std::io::Error::other(e.to_string()))?;
                writeln!(file, "{header}")?;
                file.flush()?;
            }
            Ok(file)
        };
        match open() {
            Ok(file) => self.file = Some(file),
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    "cannot open transcript (persistence disabled for this session): {e}"
                );
                // Drop the path so we warn once, not per message.
                self.path = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cupel_core::types::{
        Api, AssistantContent, AssistantMessage, Message, StopReason, TextContent, Usage, now_ms,
    };

    fn temp_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cupel-session-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn assistant_message(text: &str) -> AgentMessage {
        AgentMessage::Llm(Message::Assistant(AssistantMessage {
            content: vec![AssistantContent::Text(TextContent::plain(text))],
            api: Api::from("mock"),
            provider: cupel_core::types::Provider::from("mock"),
            model: "mock-model".into(),
            response_model: None,
            response_id: None,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: now_ms(),
        }))
    }

    fn recorder(home: &Path, cwd: &Path) -> SessionRecorder {
        SessionRecorder::new(Some(home.to_path_buf()), cwd, "cupel-42", "mock-model")
    }

    #[test]
    fn project_slug_flattens_paths() {
        assert_eq!(
            project_slug(Path::new("/Users/denny/repos/cupel")),
            "-Users-denny-repos-cupel"
        );
        assert_eq!(project_slug(Path::new("/a/b.c d_e")), "-a-b-c-d_e");
    }

    #[test]
    fn nothing_is_written_before_the_first_message() {
        let root = temp_root("lazy");
        let _recorder = recorder(&root.join("home"), &root.join("proj"));
        assert!(
            !root.join("home").exists(),
            "constructor must not touch disk"
        );
    }

    #[test]
    fn transcript_roundtrips_header_and_messages() {
        let root = temp_root("roundtrip");
        let (home, cwd) = (root.join("home"), root.join("proj"));
        let mut rec = recorder(&home, &cwd);

        let user = AgentMessage::user_text("hello");
        let assistant = assistant_message("hi there");
        let custom = AgentMessage::Custom {
            kind: "note".into(),
            payload: serde_json::json!({"x": 1}),
            timestamp: now_ms(),
        };
        rec.record(&user);
        rec.record(&assistant);
        rec.record(&custom);

        let path = sessions_dir(Some(&home), &cwd)
            .unwrap()
            .join("cupel-42.jsonl");
        let (header, messages) = load_transcript(&path).unwrap();
        assert_eq!(header.session_id, "cupel-42");
        assert_eq!(header.version, TRANSCRIPT_VERSION);
        assert_eq!(header.cwd, cwd.display().to_string());
        assert_eq!(messages, vec![user, assistant, custom]);
    }

    #[test]
    fn malformed_message_lines_are_skipped_but_bad_headers_refuse() {
        let root = temp_root("corrupt");
        let (home, cwd) = (root.join("home"), root.join("proj"));
        let mut rec = recorder(&home, &cwd);
        rec.record(&AgentMessage::user_text("one"));
        rec.record(&AgentMessage::user_text("two"));

        let path = sessions_dir(Some(&home), &cwd)
            .unwrap()
            .join("cupel-42.jsonl");
        // Corrupt the middle: truncate the second message line.
        let mut lines: Vec<String> = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(String::from)
            .collect();
        lines[2] = lines[2][..10].to_string();
        std::fs::write(&path, lines.join("\n")).unwrap();
        let (_, messages) = load_transcript(&path).unwrap();
        assert_eq!(messages.len(), 1, "corrupt line skipped, good one kept");

        // Unknown version: refuse.
        let versioned = path.with_file_name("versioned.jsonl");
        std::fs::write(
            &versioned,
            format!(
                "{}\n",
                serde_json::json!({"version": 99, "sessionId": "x", "cwd": "/", "model": "m", "startedAt": 1})
            ),
        )
        .unwrap();
        assert!(
            load_transcript(&versioned)
                .unwrap_err()
                .contains("version 99")
        );

        // Garbage header: refuse.
        let garbage = path.with_file_name("garbage.jsonl");
        std::fs::write(&garbage, "not json\n").unwrap();
        assert!(load_transcript(&garbage).is_err());
    }

    #[test]
    fn find_latest_picks_the_newest_transcript() {
        let root = temp_root("latest");
        let (home, cwd) = (root.join("home"), root.join("proj"));
        assert!(find_latest(Some(&home), &cwd).is_none(), "no dir yet");

        let dir = sessions_dir(Some(&home), &cwd).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("cupel-1.jsonl"), "old\n").unwrap();
        std::fs::write(dir.join("ignored.txt"), "not a transcript\n").unwrap();
        // Ensure a strictly newer mtime (fs timestamps can be coarse).
        let newer = dir.join("cupel-2.jsonl");
        std::fs::write(&newer, "new\n").unwrap();
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(10);
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&newer)
            .unwrap();
        file.set_modified(later).unwrap();

        assert_eq!(find_latest(Some(&home), &cwd), Some(newer));
    }

    #[test]
    fn resuming_appends_without_a_second_header() {
        let root = temp_root("resume");
        let (home, cwd) = (root.join("home"), root.join("proj"));
        let mut first = recorder(&home, &cwd);
        first.record(&AgentMessage::user_text("first session"));
        drop(first);

        // Same session id = same file: the header must not repeat.
        let mut resumed = recorder(&home, &cwd);
        resumed.record(&AgentMessage::user_text("resumed"));

        let path = sessions_dir(Some(&home), &cwd)
            .unwrap()
            .join("cupel-42.jsonl");
        let content = std::fs::read_to_string(&path).unwrap();
        assert_eq!(content.matches("\"version\":").count(), 1, "one header");
        let (_, messages) = load_transcript(&path).unwrap();
        assert_eq!(messages.len(), 2);
    }

    #[tokio::test]
    async fn disabled_recorder_is_a_silent_no_op() {
        let root = temp_root("disabled");
        let mut rec = SessionRecorder::new(None, &root, "cupel-42", "mock-model");
        rec.before_prompt("hello").await;
        rec.record(&AgentMessage::user_text("hello"));
        rec.on_steer("steer");
        rec.on_agent_end();
        rec.end_session().await;
        // Nothing anywhere on disk (the temp root stays empty).
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
    }
}
