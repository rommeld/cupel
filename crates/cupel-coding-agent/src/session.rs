//! Session persistence, every conversation is recorded as a JSONL
//! transcript so it can be resumed (the resume option).
//! Project lifecycle hooks require an explicit, home stored trust grant.
//!
//! Layout, `<cupel home>/sessions/<project slug>/<session id>.jsonl`, where
//! the slug preserves ASCII letters, digits, underscores and hyphens,
//! replacing every other cwd character with a hyphen
//! (the same scheme Claude Code uses for `~/.claude/projects/`). Line 1 is
//! a header object, every following line is one [`AgentMessage`] in its
//! existing serde form. Appends are flushed per message, so a crash loses
//! at most the in flight message.
//!
//! Like `.cupel/` scaffolding, the transcript is created lazily on the
//! first agent interaction. Launching and quitting cupel leaves no trace.
//! Persisted history is never rewritten by compaction (compaction only
//! mutates a run's private context snapshot), so a transcript is always the
//! full conversation, resume replays it and the loop re compacts as needed.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use cupel_agent::AgentMessage;
use serde::{Deserialize, Serialize};

use crate::hooks::{HookEvent, HookRunner};

/// Bumped when the transcript format changes incompatibly, the loader
/// refuses to resume a version it doesn't understand.
pub const TRANSCRIPT_VERSION: u32 = 1;

/// Line 1 of every transcript, session facts at start. Deliberately not
/// updated afterwards (a mid session `/model` switch is visible in the
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

/// The cwd as a filesystem safe directory name, ASCII letters, digits,
/// underscores and hyphens are preserved. All other characters become hyphens.
/// Paths differing only by separators and hyphens can collide. This is
/// acceptable for a per user layout because the header records the real cwd.
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

/// `<home>/sessions/<slug>`. `None` (no resolvable home) disables
/// persistence entirely.
#[must_use]
pub fn sessions_dir(home: Option<&Path>, cwd: &Path) -> Option<PathBuf> {
    Some(home?.join("sessions").join(project_slug(cwd)))
}

/// The newest transcript (by modification time) for this project is what a
/// bare the resume option picks.
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

/// Move every transcript of the project at `from_cwd` into `archive/` in the sessins
/// directory of the project at `into_cwd`, then remove the emptied directory.
/// Listings, the hot reload completion and a bare the resume option read only top level
/// files, so archived sessions leave all of them, moving a file back restores it. A
/// name the archive has gets a number appended after a hyphen, so no file is ever
/// overwritten. Returns how many transcripts moved.
pub fn archive_sessions(
    home: Option<&Path>,
    from_cwd: &Path,
    into_cwd: &Path,
) -> std::io::Result<usize> {
    let (Some(from), Some(into)) = (sessions_dir(home, from_cwd), sessions_dir(home, into_cwd))
    else {
        return Ok(0);
    };
    let entries = match std::fs::read_dir(&from) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };
    let transcripts: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "jsonl"))
        .collect();
    let archive = into.join("archive");
    std::fs::create_dir_all(&archive)?;
    for transcript in &transcripts {
        let stem = transcript.file_stem().unwrap_or_default().to_string_lossy();
        let target = free_name(&archive, &stem);
        let artifacts = transcript.with_extension("");
        let archived_artifacts = target.with_extension("");
        let moved_artifacts = artifacts.is_dir();
        if moved_artifacts {
            std::fs::rename(&artifacts, &archived_artifacts)?;
        }
        if let Err(error) = std::fs::rename(transcript, &target) {
            if moved_artifacts {
                let _ = std::fs::rename(&archived_artifacts, &artifacts);
            }
            return Err(error);
        }
    }
    let _ = std::fs::remove_dir(&from);
    Ok(transcripts.len())
}

/// The first unused name in `dir`, starting with `<stem>.jsonl` and then
/// appending a hyphen and increasing number to the stem until it is free.
fn free_name(dir: &Path, stem: &str) -> PathBuf {
    let mut target = dir.join(format!("{stem}.jsonl"));
    let mut number = 0;
    while target.exists() || target.with_extension("").exists() {
        number += 1;
        target = dir.join(format!("{stem}-{number}.jsonl"));
    }
    target
}

/// One row of the session listing. There is no stored summary in a
/// transcript (compaction summaries live only inside a run's context
/// snapshot and are never persisted), so the closest human readable label
/// is the session's first user prompt, plus countable facts.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionSummary {
    pub id: String,
    pub path: PathBuf,
    pub prompt: String,
    pub started_at: u64,
    pub model: String,
    pub message_count: usize,
    pub label: String,
}

/// How many chars of the first prompt the listing shows.
const LABEL_MAX_CHARS: usize = 60;

/// Summarize every readable transcript in `dir`, newest first (by header
/// start time). Unreadable files are skipped with a warning because one corrupt
/// transcript must not hide the rest of the listing.
#[must_use]
pub fn list_sessions_in(dir: &Path) -> Vec<SessionSummary> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new(); // no sessions dir yet = nothing to list
    };
    let mut sessions: Vec<SessionSummary> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "jsonl"))
        .filter_map(|path| match load_transcript(&path) {
            Ok((header, messages)) => Some(summarize(path, &header, &messages)),
            Err(e) => {
                tracing::warn!(path = %path.display(), "skipping unreadable transcript: {e}");
                None
            }
        })
        .collect();
    sessions.sort_by_key(|s| std::cmp::Reverse(s.started_at));
    sessions
}

/// Derive a listing row from a parsed transcript.
fn summarize(
    path: PathBuf,
    header: &TranscriptHeader,
    messages: &[AgentMessage],
) -> SessionSummary {
    // The first LLM user message is the session's opening prompt.
    let prompt = messages
        .iter()
        .find_map(|m| match m {
            AgentMessage::Llm(cupel_core::types::Message::User(user)) => match &user.content {
                cupel_core::types::UserContentBody::Text(text) => Some(text.clone()),
                cupel_core::types::UserContentBody::Blocks(blocks) => Some(
                    blocks
                        .iter()
                        .filter_map(|block| match block {
                            cupel_core::types::UserContent::Text(text) => Some(text.text.as_str()),
                            cupel_core::types::UserContent::Image(_) => None,
                        })
                        .collect::<Vec<_>>()
                        .join(" "),
                ),
            },
            _ => None,
        })
        .unwrap_or_default();
    let prompt = normalize_prompt(&prompt);
    // Truncate on character boundaries (a byte slice could split a multi byte
    // character and panic). Searching always uses the full opening prompt.
    let label: String = prompt.chars().take(LABEL_MAX_CHARS).collect();
    SessionSummary {
        id: header.session_id.clone(),
        path,
        prompt,
        started_at: header.started_at,
        model: header.model.clone(),
        message_count: messages.len(),
        label,
    }
}

fn normalize_prompt(prompt: &str) -> String {
    prompt.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Find sessions by a case-insensitive substring of their full opening prompt.
/// Empty searches list every readable transcript, newest first.
#[must_use]
pub fn search_sessions_in(dir: &Path, query: &str) -> Vec<SessionSummary> {
    let query = normalize_prompt(query).to_lowercase();
    list_sessions_in(dir)
        .into_iter()
        .filter(|session| session.prompt.to_lowercase().contains(&query))
        .collect()
}

/// Resolve a unique opening-prompt search. Exact internal keys remain accepted
/// for existing restart commands and scripts, but are not exposed in listings.
pub fn find_session_in(dir: &Path, query: &str) -> Result<PathBuf, String> {
    let sessions = list_sessions_in(dir);
    if let Some(session) = sessions.iter().find(|session| session.id == query) {
        return Ok(session.path.clone());
    }
    let query = normalize_prompt(query).to_lowercase();
    if query.is_empty() {
        return Err("provide an opening-prompt search (/sessions lists saved sessions)".into());
    }
    let matches: Vec<_> = sessions
        .into_iter()
        .filter(|session| session.prompt.to_lowercase().contains(&query))
        .collect();
    match matches.as_slice() {
        [session] => Ok(session.path.clone()),
        [] => Err(format!(
            "no session matches {query:?} for this project (/sessions lists them)"
        )),
        _ => Err(format!(
            "{} sessions match {query:?}; use a more specific opening-prompt search (/sessions {query})",
            matches.len()
        )),
    }
}

/// Unix milliseconds → year, month and day separated by hyphens (UTC), for the session listing.
/// Hand rolled instead of a chrono dependency, the civil from days
/// algorithm (Howard Hinnant's) is ~10 lines and easily tested.
#[must_use]
pub fn date_ymd(ms: u64) -> String {
    let days = (ms / 86_400_000).cast_signed();
    // Shift the epoch from 1970 01 01 to 0000 03 01 so leap days land at
    // the end of the counting year, making month lengths regular.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097); // day of 400 year era
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153; // March based month 0..11
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

/// Parse a transcript, header off line 1, one message per following line.
/// Malformed message lines are skipped with a warning (a crash mid append
/// can truncate the last line), a malformed or wrong version header is an
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
            // +2, 1 based and the header consumed line 1.
            Err(e) => tracing::warn!(line = i + 2, "skipping malformed transcript line: {e}"),
        }
    }
    Ok((header, messages))
}

/// Owns one session's transcript file and hook dispatch. Constructed
/// unconditionally by `main` and threaded into the frontends, with no
/// resolvable cupel home it degrades to a no op (hooks still run, since they
/// only need directories, not the home).
pub struct SessionRecorder {
    header: TranscriptHeader,
    path: Option<PathBuf>,
    file: Option<std::fs::File>,
    persistence_error: Option<String>,
    hooks: HookRunner,
    started: bool,
    undo: std::sync::Arc<crate::undo::UndoJournal>,
}

impl SessionRecorder {
    /// Pure constructor that touches no filesystem. `home` should be
    /// `config_home`.
    #[must_use]
    pub fn new(home: Option<PathBuf>, cwd: &Path, session_id: &str, model_id: &str) -> Self {
        let path =
            sessions_dir(home.as_deref(), cwd).map(|d| d.join(format!("{session_id}.jsonl")));
        // Home hooks are user installed. Repository executables are never
        // loaded merely because cupel was launched inside a checkout.
        let mut hook_roots = Vec::new();
        if let Some(home) = &home {
            hook_roots.push(home.clone());
        }
        if crate::project_trust::is_trusted(home.as_deref(), cwd) {
            hook_roots.push(cwd.join(".cupel"));
        }
        let session_ref = path.clone().unwrap_or_default();
        Self {
            header: TranscriptHeader {
                version: TRANSCRIPT_VERSION,
                session_id: session_id.to_string(),
                cwd: cwd.display().to_string(),
                model: model_id.to_string(),
                started_at: cupel_core::types::now_ms(),
            },
            undo: crate::undo::shared(path.as_deref(), cwd),
            path,
            file: None,
            persistence_error: None,
            hooks: HookRunner::new(hook_roots, session_id, &session_ref, cwd),
            started: false,
        }
    }

    #[must_use]
    pub fn session_id(&self) -> &str {
        &self.header.session_id
    }

    /// The directory holding this project's transcripts (parent of this
    /// session's file), `None` when persistence is disabled. Threading it
    /// from here keeps the TUI's session listing env free and testable.
    #[must_use]
    pub fn sessions_dir(&self) -> Option<&Path> {
        self.path.as_deref().and_then(Path::parent)
    }

    /// This session's transcript file, whether it exists yet or not (it appears with
    /// the first message), `None` when persistence is off.
    #[must_use]
    pub fn transcript(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    #[must_use]
    pub fn undo(&self) -> std::sync::Arc<crate::undo::UndoJournal> {
        std::sync::Arc::clone(&self.undo)
    }

    /// Let stop hooks finish before inspecting or restoring the workspace.
    pub async fn settle_hooks(&mut self) {
        self.hooks.settle().await;
    }

    /// A prompt is about to start a run. Settles any pending `stop` hook
    /// first (ordering guarantee for external consumers), lazily creates
    /// the transcript, then fires session start (once) and
    /// user prompt submission, both awaited and bounded by the hook timeout.
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

    /// A prompt was queued mid run, fire its hook now, at submit time, in the
    /// background (the TUI key handler must not block on hook processes).
    pub fn on_queued_prompt(&mut self, prompt: &str) {
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

    /// Commit one complete lifecycle record before releasing its resources.
    /// Deliberately unsaved sessions succeed in memory; failed persistence does not.
    pub fn record_checked(&mut self, message: &AgentMessage) -> Result<(), String> {
        if let Some(problem) = &self.persistence_error {
            return Err(problem.clone());
        }
        let persistent = self.path.is_some();
        self.ensure_file();
        if let Some(problem) = &self.persistence_error {
            return Err(problem.clone());
        }
        if !persistent {
            return Ok(());
        }
        let file = self
            .file
            .as_mut()
            .ok_or("transcript persistence is unavailable")?;
        let previous_len = file
            .metadata()
            .map_err(|error| format!("cannot inspect transcript before commit: {error}"))?
            .len();
        let mut line = serde_json::to_vec(message)
            .map_err(|error| format!("cannot encode lifecycle record: {error}"))?;
        line.push(b'\n');
        if let Err(error) = file
            .write_all(&line)
            .and_then(|()| file.flush())
            .and_then(|()| file.sync_data())
        {
            let original = format!("cannot commit transcript lifecycle record: {error}");
            if let Err(rollback) = file.set_len(previous_len).and_then(|()| file.sync_data()) {
                let problem = format!("{original}; cannot restore transcript prefix: {rollback}");
                self.persistence_error = Some(problem.clone());
                self.file = None;
                return Err(problem);
            }
            return Err(original);
        }
        Ok(())
    }

    /// The agent run finished, fire `stop` without blocking the frontend.
    pub fn on_agent_end(&mut self) {
        self.hooks.fire_background(HookEvent::Stop);
    }

    /// End the session, drain its hooks, and remove its undo snapshots.
    /// Used for session replacement as well as normal frontend shutdown.
    /// (A killed process skips this, but the transcript is still intact thanks
    /// to per message flushing.)
    pub async fn end_session(&mut self) {
        // Only a session that actually started (had a prompt) announces an
        // end. Launch+quit stays completely silent.
        if self.started {
            self.hooks.dispatch(HookEvent::SessionEnd).await;
        }
        if let Err(error) = self.undo.cleanup().await {
            tracing::warn!(session_id = %self.header.session_id, "{error}");
        }
    }

    /// Open the transcript for appending, writing the header if (and only
    /// if) the file is new. On resume the original header is kept. Every
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
                self.persistence_error =
                    Some(format!("cannot open transcript {}: {e}", path.display()));
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
            error_kind: None,
            timestamp: now_ms(),
        }))
    }

    #[test]
    fn checked_lifecycle_record_is_one_durable_replayable_message() {
        let home = temp_root("checked-lifecycle");
        let mut recorder = SessionRecorder::new(Some(home.clone()), &home, "checked", "mock");
        let transition = AgentMessage::Custom {
            kind: "fixture.lifecycle".into(),
            payload: serde_json::json!({"state":"complete"}),
            timestamp: 1,
        };
        recorder.record_checked(&transition).unwrap();
        let (_, messages) = load_transcript(recorder.transcript().unwrap()).unwrap();
        assert_eq!(messages, vec![transition]);
    }

    #[test]
    fn checked_lifecycle_record_distinguishes_unsaved_and_failed_persistence() {
        let home = temp_root("checked-disabled");
        let mut unsaved = SessionRecorder::new(None, &home, "unsaved", "mock");
        let message = AgentMessage::user_text("transition");
        assert!(unsaved.record_checked(&message).is_ok());
        let mut failed = SessionRecorder::new(Some(home.clone()), &home, "failed", "mock");
        std::fs::create_dir_all(failed.transcript().unwrap()).unwrap();
        assert!(failed.record_checked(&message).is_err());
        assert!(failed.transcript().is_none());
        assert!(
            failed.record_checked(&message).is_err(),
            "a failed save must not become an intentional unsaved session"
        );
    }

    #[test]
    fn checked_append_failure_keeps_the_valid_prefix_and_blocks_later_commits() {
        let home = temp_root("checked-prefix");
        let mut recorder = SessionRecorder::new(Some(home.clone()), &home, "prefix", "mock");
        recorder
            .record_checked(&AgentMessage::user_text("kept"))
            .unwrap();
        let path = recorder.transcript().unwrap().to_path_buf();
        let prefix = std::fs::read(&path).unwrap();
        recorder.file = Some(std::fs::File::open(&path).unwrap());
        assert!(
            recorder
                .record_checked(&AgentMessage::user_text("rejected"))
                .is_err()
        );
        assert_eq!(std::fs::read(&path).unwrap(), prefix);
        assert!(
            recorder
                .record_checked(&AgentMessage::user_text("still rejected"))
                .is_err()
        );
    }

    fn recorder(home: &Path, cwd: &Path) -> SessionRecorder {
        SessionRecorder::new(Some(home.to_path_buf()), cwd, "cupel-42", "mock-model")
    }

    #[test]
    fn date_ymd_converts_known_dates() {
        assert_eq!(date_ymd(0), "1970-01-01");
        // 2026 07 13 000000 UTC (verified against the date command in UTC) and a leap day.
        assert_eq!(date_ymd(1_783_900_800_000), "2026-07-13");
        assert_eq!(date_ymd(1_709_164_800_000), "2024-02-29");
    }

    #[test]
    fn session_listing_is_newest_first_with_first_prompt_labels() {
        let root = temp_root("listing");
        let dir = root.join("sessions");
        std::fs::create_dir_all(&dir).unwrap();
        // Two hand written transcripts with different start times.
        let write = |id: &str, started: u64, prompt: &str| {
            let header = serde_json::to_string(&TranscriptHeader {
                version: TRANSCRIPT_VERSION,
                session_id: id.into(),
                cwd: "/proj".into(),
                model: "mock-model".into(),
                started_at: started,
            })
            .unwrap();
            let user = serde_json::to_string(&AgentMessage::user_text(prompt)).unwrap();
            let reply = serde_json::to_string(&assistant_message("done")).unwrap();
            std::fs::write(
                dir.join(format!("{id}.jsonl")),
                format!("{header}\n{user}\n{reply}\n"),
            )
            .unwrap();
        };
        write(
            "cupel-1",
            1_000,
            "fix the login bug in a very long prompt that should be truncated at sixty characters exactly",
        );
        write("cupel-2", 2_000, "short one");
        // A corrupt file must not break the listing.
        std::fs::write(dir.join("cupel-3.jsonl"), "{broken").unwrap();

        let sessions = list_sessions_in(&dir);
        assert_eq!(sessions.len(), 2, "corrupt transcript skipped");
        assert_eq!(sessions[0].id, "cupel-2", "newest first");
        assert_eq!(sessions[0].label, "short one");
        assert_eq!(sessions[0].message_count, 2);
        assert_eq!(sessions[1].id, "cupel-1");
        assert_eq!(sessions[1].label.chars().count(), 60, "label truncated");
        assert_eq!(sessions[1].model, "mock-model");
        // No sessions dir at all, empty, not an error.
        assert!(list_sessions_in(&root.join("missing")).is_empty());
    }

    #[test]
    fn opening_prompt_search_uses_full_text_and_preserves_old_transcripts() {
        let root = temp_root("search");
        let mut first = SessionRecorder::new(Some(root.clone()), &root, "legacy-key", "mock");
        let prompt = format!("{}\nFind\t the 登录 bug", "é".repeat(65));
        first.record(&AgentMessage::user_text(prompt));
        first.record(&AgentMessage::user_text("later prompt must not match"));
        let path = first.transcript().unwrap().to_path_buf();
        let original = std::fs::read(&path).unwrap();
        let dir = first.sessions_dir().unwrap();
        std::fs::write(dir.join("broken.jsonl"), "not json").unwrap();

        let matches = search_sessions_in(dir, "  FIND\n the   登录  ");
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].label.chars().count(), LABEL_MAX_CHARS);
        assert!(
            !matches[0].label.contains("Find"),
            "search extends beyond the label"
        );
        assert_eq!(find_session_in(dir, "登录 BUG").unwrap(), path);
        assert_eq!(
            find_session_in(dir, "legacy-key").unwrap(),
            path,
            "old restart commands still work"
        );
        assert!(search_sessions_in(dir, "later prompt").is_empty());
        assert!(
            find_session_in(dir, "absent")
                .unwrap_err()
                .contains("no session matches")
        );
        assert!(
            find_session_in(dir, "../legacy-key").is_err(),
            "search is never a filesystem path"
        );
        assert!(
            find_session_in(dir, " \n ").is_err(),
            "an empty search cannot resume arbitrarily"
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            original,
            "discovery never rewrites legacy transcripts"
        );
    }

    #[test]
    fn ambiguous_opening_prompts_never_resume_an_arbitrary_session() {
        let root = temp_root("ambiguous-search");
        for (key, prompt) in [
            ("one", "fix login bug"),
            ("two", "fix logout bug"),
            ("three", "fix login bug"),
        ] {
            let mut recorder = SessionRecorder::new(Some(root.clone()), &root, key, "mock");
            recorder.record(&AgentMessage::user_text(prompt));
        }
        let dir = sessions_dir(Some(&root), &root).unwrap();
        assert_eq!(search_sessions_in(&dir, "bug").len(), 3);
        assert!(
            find_session_in(&dir, "bug")
                .unwrap_err()
                .contains("3 sessions match")
        );
        assert!(
            find_session_in(&dir, "fix login bug")
                .unwrap_err()
                .contains("2 sessions match")
        );
        assert_eq!(
            find_session_in(&dir, "logout").unwrap(),
            dir.join("two.jsonl")
        );
        assert!(search_sessions_in(&root.join("missing"), "bug").is_empty());
    }

    #[test]
    fn opening_prompt_search_includes_text_blocks_without_using_later_messages() {
        use cupel_core::types::{UserContent, UserContentBody, UserMessage};

        let root = temp_root("block-search");
        let mut recorder = recorder(&root, &root);
        recorder.record(&AgentMessage::Llm(Message::User(UserMessage {
            content: UserContentBody::Blocks(vec![
                UserContent::Text(TextContent::plain("Explain this")),
                UserContent::Text(TextContent::plain("screenshot")),
            ]),
            timestamp: 1,
        })));
        recorder.record(&AgentMessage::user_text("later request"));
        let dir = recorder.sessions_dir().unwrap();
        assert_eq!(
            search_sessions_in(dir, "THIS screenshot")[0].prompt,
            "Explain this screenshot"
        );
        assert!(search_sessions_in(dir, "later request").is_empty());
    }

    #[test]
    fn recorder_exposes_its_sessions_dir() {
        let root = temp_root("dir");
        let rec = recorder(&root.join("home"), &root.join("proj"));
        let dir = rec.sessions_dir().expect("home given, dir known");
        assert!(dir.ends_with(format!("sessions/{}", project_slug(&root.join("proj")))));
        let path = dir.join("cupel-42.jsonl");
        assert_eq!(rec.transcript(), Some(path.as_path()));
        assert!(!path.exists(), "the transcript is created lazily");
        // Disabled persistence (no home), no dir either.
        let disabled = SessionRecorder::new(None, &root.join("proj"), "cupel-1", "m");
        assert!(disabled.sessions_dir().is_none());
        assert!(disabled.transcript().is_none());
    }

    #[test]
    fn project_slug_flattens_paths() {
        assert_eq!(
            project_slug(Path::new("/workspace/repos/project")),
            "-workspace-repos-project"
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
        // Corrupt the middle, truncate the second message line.
        let mut lines: Vec<String> = std::fs::read_to_string(&path)
            .unwrap()
            .lines()
            .map(String::from)
            .collect();
        lines[2] = lines[2][..10].to_string();
        std::fs::write(&path, lines.join("\n")).unwrap();
        let (_, messages) = load_transcript(&path).unwrap();
        assert_eq!(messages.len(), 1, "corrupt line skipped, good one kept");

        // Unknown version, refuse.
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

        // Garbage header, refuse.
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

        // Same session id = same file, the header must not repeat.
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
        rec.on_queued_prompt("queued");
        rec.on_agent_end();
        rec.end_session().await;
        // Nothing anywhere on disk (the temp root stays empty).
        assert_eq!(std::fs::read_dir(&root).unwrap().count(), 0);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn project_hooks_require_explicit_trust_for_every_event() {
        use crate::project_trust::{ProjectTrust, save};
        use std::os::unix::fs::PermissionsExt as _;

        let root = temp_root("hook-trust");
        let (home, cwd) = (root.join("home"), root.join("proj"));
        let events = ["session-start", "user-prompt-submit", "stop", "session-end"];
        for (hook_root, log) in [
            (home.clone(), root.join("home.log")),
            (cwd.join(".cupel"), root.join("project.log")),
        ] {
            for event in events {
                let dir = hook_root.join("hooks").join(event);
                std::fs::create_dir_all(&dir).unwrap();
                let script = dir.join("capture");
                std::fs::write(
                    &script,
                    format!("#!/bin/sh\necho {event} >> '{}'\n", log.display()),
                )
                .unwrap();
                std::fs::set_permissions(script, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
        }

        for trust in [None, Some(ProjectTrust::Restricted)] {
            if let Some(trust) = trust {
                save(&home, &cwd, trust).unwrap();
            }
            let mut rec = recorder(&home, &cwd);
            rec.before_prompt("first prompt").await;
            rec.on_queued_prompt("queued prompt");
            rec.on_agent_end();
            rec.end_session().await;
            assert!(
                root.join("home.log").exists(),
                "user-installed hooks still run"
            );
            assert!(
                !root.join("project.log").exists(),
                "no project code may run without trust"
            );
        }

        save(&home, &cwd, ProjectTrust::Trusted).unwrap();
        let mut rec = recorder(&home, &cwd);
        rec.before_prompt("first prompt").await;
        rec.on_queued_prompt("queued prompt");
        rec.on_agent_end();
        rec.end_session().await;
        assert_eq!(
            std::fs::read_to_string(root.join("project.log")).unwrap(),
            "session-start\nuser-prompt-submit\nuser-prompt-submit\nstop\nsession-end\n"
        );

        // No home means no possible trust grant, not implicit permission.
        std::fs::remove_file(root.join("project.log")).unwrap();
        let mut rec = SessionRecorder::new(None, &cwd, "no-home", "mock-model");
        rec.before_prompt("prompt").await;
        rec.end_session().await;
        assert!(!root.join("project.log").exists());
    }

    #[test]
    fn archived_transcripts_leave_the_listings_and_never_overwrite() {
        let root = temp_root("archive");
        let home = root.join("home");
        let origin = root.join("proj");
        let spinoff = origin.join(".cupel/worktrees/auth");
        let from = sessions_dir(Some(&home), &spinoff).unwrap();
        std::fs::create_dir_all(&from).unwrap();
        for id in ["cupel-1", "cupel-2"] {
            std::fs::write(from.join(format!("{id}.jsonl")), id).unwrap();
        }
        // An earlier spinoff left a cupel 1 in the archive.
        let archive = sessions_dir(Some(&home), &origin).unwrap().join("archive");
        std::fs::create_dir_all(&archive).unwrap();
        std::fs::write(archive.join("cupel-1.jsonl"), "older").unwrap();
        std::fs::create_dir_all(from.join("cupel-1")).unwrap();
        std::fs::write(from.join("cupel-1/custom_42.jsonl"), "helper transcript").unwrap();

        assert_eq!(archive_sessions(Some(&home), &spinoff, &origin).unwrap(), 2);
        assert!(!from.exists(), "the emptied directory is gone");
        assert_eq!(
            std::fs::read_to_string(archive.join("cupel-1-1/custom_42.jsonl")).unwrap(),
            "helper transcript"
        );
        let read = |name: &str| std::fs::read_to_string(archive.join(name)).unwrap();
        assert_eq!(read("cupel-1.jsonl"), "older");
        assert_eq!(read("cupel-1-1.jsonl"), "cupel-1");
        assert_eq!(read("cupel-2.jsonl"), "cupel-2");
        // A bare resume in the origin doesn't look into the archive.
        assert!(find_latest(Some(&home), &origin).is_none());
        // Nothing recorded, or no home, nothing to move.
        assert_eq!(archive_sessions(Some(&home), &spinoff, &origin).unwrap(), 0);
        assert_eq!(archive_sessions(None, &spinoff, &origin).unwrap(), 0);
    }
}
