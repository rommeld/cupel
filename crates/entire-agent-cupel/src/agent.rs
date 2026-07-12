//! The cupel-specific protocol handlers.
//!
//! Everything session-shaped is delegated to `cupel_coding_agent::session`
//! (transcript format, sessions dir, project slug) so this shim can never
//! drift from what cupel actually writes. What remains here is translation:
//! cupel's JSONL transcript and hook payloads in, Entire's protocol JSON out.

use std::path::{Path, PathBuf};

use base64::Engine as _;
use cupel_agent::AgentMessage;
use cupel_coding_agent::session;
use cupel_core::types::{AssistantContent, Message, UserContentBody};

use crate::types::{
    self, AgentSession, Capabilities, CupelHookPayload, Event, HookInput, InfoResponse,
};

/// The four cupel hook events the forwarding scripts relay to Entire. Also
/// the `hook_names` advertised by `info` and the names `parse-hook` accepts.
pub const HOOK_EVENTS: [&str; 4] = ["session-start", "user-prompt-submit", "stop", "session-end"];

/// The repository Entire is operating on: it always sets ENTIRE_REPO_ROOT
/// (and the working directory) when invoking an agent; cwd is the fallback
/// for running subcommands by hand.
pub fn repo_root() -> PathBuf {
    std::env::var_os("ENTIRE_REPO_ROOT")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_default())
}

pub fn info() -> InfoResponse {
    InfoResponse {
        protocol_version: types::PROTOCOL_VERSION,
        name: "cupel",
        agent_type: "Cupel",
        description: "cupel coding agent integration for Entire",
        is_preview: true,
        protected_dirs: vec![".cupel"],
        hook_names: HOOK_EVENTS.to_vec(),
        capabilities: Capabilities {
            hooks: true,
            transcript_analyzer: true,
            transcript_preparer: false,
            token_calculator: false,
            compact_transcript: false,
            text_generator: false,
            hook_response_writer: false,
            subagent_aware_extractor: false,
            uses_terminal: true,
        },
    }
}

/// `detect`: is the cupel binary on PATH?
pub fn detect() -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| is_executable(&dir.join("cupel")))
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// `get-session-dir`: where cupel keeps this repo's transcripts.
pub fn session_dir(repo_path: &Path) -> Result<PathBuf, String> {
    session::sessions_dir(
        cupel_coding_agent::resources::config_home().as_deref(),
        repo_path,
    )
    .ok_or_else(|| "no cupel home resolvable (set CUPEL_HOME or HOME)".to_string())
}

/// `resolve-session-file`: cupel names transcripts `<session-id>.jsonl`.
pub fn resolve_session_file(session_dir: &Path, session_id: &str) -> PathBuf {
    session_dir.join(format!("{session_id}.jsonl"))
}

/// `format-resume-command`: how a user (or Entire) reopens a session.
pub fn format_resume_command(session_id: &str) -> String {
    if session_id.is_empty() {
        "cupel --resume".to_string()
    } else {
        format!("cupel --resume {session_id}")
    }
}

/// `read-session`: assemble Entire's cross-agent session record from a
/// cupel transcript. `native_data` carries the raw transcript bytes
/// (base64, matching Go's `[]byte` marshaling) so `write-session` can
/// restore the file verbatim.
pub fn read_session(input: &HookInput) -> Result<AgentSession, String> {
    let session_ref = if input.session_ref.is_empty() {
        if input.session_id.is_empty() {
            return Err("session_ref or session_id is required".to_string());
        }
        resolve_session_file(&session_dir(&repo_root())?, &input.session_id)
    } else {
        PathBuf::from(&input.session_ref)
    };

    let bytes = std::fs::read(&session_ref)
        .map_err(|e| format!("cannot read transcript {}: {e}", session_ref.display()))?;
    let (header, messages) = session::load_transcript(&session_ref)?;

    Ok(AgentSession {
        session_id: header.session_id,
        agent_name: "cupel".to_string(),
        repo_path: repo_root().display().to_string(),
        session_ref: session_ref.display().to_string(),
        start_time: types::rfc3339_utc(header.started_at),
        native_data: Some(base64::engine::general_purpose::STANDARD.encode(&bytes)),
        modified_files: modified_files(&messages, 0),
        new_files: Vec::new(),
        deleted_files: Vec::new(),
    })
}

/// `write-session`: restore the transcript bytes to `session_ref` (used by
/// Entire when rewinding to a checkpoint).
pub fn write_session(session: &AgentSession) -> Result<(), String> {
    if session.session_ref.is_empty() {
        return Err("session_ref is required".to_string());
    }
    let bytes = match &session.native_data {
        Some(data) => base64::engine::general_purpose::STANDARD
            .decode(data)
            .map_err(|e| format!("native_data is not valid base64: {e}"))?,
        None => Vec::new(),
    };
    let path = Path::new(&session.session_ref);
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    std::fs::write(path, bytes).map_err(|e| e.to_string())
}

/// `chunk-transcript`: split into base64 chunks of at most `max_size` raw
/// bytes each.
pub fn chunk_transcript(content: &[u8], max_size: usize) -> Result<Vec<String>, String> {
    if max_size == 0 {
        return Err("max-size must be positive".to_string());
    }
    Ok(content
        .chunks(max_size)
        .map(|chunk| base64::engine::general_purpose::STANDARD.encode(chunk))
        .collect())
}

/// `reassemble-transcript`: the inverse of `chunk_transcript`.
pub fn reassemble_transcript(chunks: &[String]) -> Result<Vec<u8>, String> {
    let mut data = Vec::new();
    for chunk in chunks {
        data.extend(
            base64::engine::general_purpose::STANDARD
                .decode(chunk)
                .map_err(|e| format!("chunk is not valid base64: {e}"))?,
        );
    }
    Ok(data)
}

// ---------------------------------------------------------------------------
// Transcript analysis
//
// "Position" is deliberately the MESSAGE COUNT (lines after the header), not
// a byte offset: it survives pretty-printing-free appends and lets `offset`
// mean "skip the first N messages I already processed".
// ---------------------------------------------------------------------------

/// `get-transcript-position`: a missing transcript is position 0, not an
/// error (Entire probes before the first prompt has created the file).
pub fn transcript_position(path: &Path) -> Result<usize, String> {
    if !path.exists() {
        return Ok(0);
    }
    let (_, messages) = session::load_transcript(path)?;
    Ok(messages.len())
}

/// `extract-modified-files`: paths named by write/edit tool calls from
/// message `offset` on, deduped and sorted, plus the new position.
pub fn extract_modified_files(path: &Path, offset: usize) -> Result<(Vec<String>, usize), String> {
    let (_, messages) = session::load_transcript(path)?;
    let files = modified_files(&messages, offset);
    Ok((files, messages.len()))
}

/// `extract-prompts`: user prompt texts from message `offset` on.
pub fn extract_prompts(path: &Path, offset: usize) -> Result<Vec<String>, String> {
    let (_, messages) = session::load_transcript(path)?;
    Ok(messages
        .iter()
        .skip(offset)
        .filter_map(|message| match message {
            AgentMessage::Llm(Message::User(user)) => match &user.content {
                UserContentBody::Text(text) => Some(text.clone()),
                UserContentBody::Blocks(_) => None,
            },
            _ => None,
        })
        .collect())
}

/// The files cupel's write/edit tools were asked to touch. Both tools take
/// the target as a top-level `path` argument (see tools/write.rs, edit.rs).
/// bash could modify anything, but guessing from shell strings would be
/// noise - Entire's git-based checkpointing catches the actual changes.
fn modified_files(messages: &[AgentMessage], offset: usize) -> Vec<String> {
    let mut files: Vec<String> = messages
        .iter()
        .skip(offset)
        .filter_map(|message| match message {
            AgentMessage::Llm(Message::Assistant(assistant)) => Some(&assistant.content),
            _ => None,
        })
        .flatten()
        .filter_map(|content| match content {
            AssistantContent::ToolCall(call) if call.name == "write" || call.name == "edit" => {
                call.arguments["path"].as_str().map(ToString::to_string)
            }
            _ => None,
        })
        .collect();
    files.sort();
    files.dedup();
    files
}

// ---------------------------------------------------------------------------
// Hooks
// ---------------------------------------------------------------------------

/// The forwarding script `install-hooks` drops into cupel's project hook
/// directories. cupel pipes the event payload to the script's stdin; `exec`
/// hands that same stdin straight to the Entire CLI.
fn forward_script(event: &str) -> String {
    format!(
        "#!/bin/sh\n\
         # Installed by entire-agent-cupel (`install-hooks`). Forwards cupel's\n\
         # {event} hook payload to the Entire CLI. Remove via `uninstall-hooks`.\n\
         exec entire hooks cupel {event}\n"
    )
}

fn hook_script_path(repo: &Path, event: &str) -> PathBuf {
    repo.join(".cupel/hooks").join(event).join("entire")
}

/// `install-hooks`: one forwarding script per event under the PROJECT's
/// `.cupel/hooks/` (project-scoped, like Entire's own opt-in settings).
/// Returns how many scripts were written.
pub fn install_hooks(repo: &Path, force: bool) -> Result<usize, String> {
    if !force && are_hooks_installed(repo) {
        return Ok(0);
    }
    for event in HOOK_EVENTS {
        let path = hook_script_path(repo, event);
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        std::fs::write(&path, forward_script(event)).map_err(|e| e.to_string())?;
        // cupel only runs hooks with an execute bit set.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))
                .map_err(|e| e.to_string())?;
        }
    }
    Ok(HOOK_EVENTS.len())
}

/// `uninstall-hooks`: remove the scripts and prune directories that are
/// left empty (only ours - a user's own hooks stay untouched).
pub fn uninstall_hooks(repo: &Path) -> Result<(), String> {
    for event in HOOK_EVENTS {
        let path = hook_script_path(repo, event);
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("cannot remove {}: {e}", path.display())),
        }
        // Prune `<event>/` then `hooks/` if now empty; leave `.cupel/`.
        let parents = [path.parent(), path.parent().and_then(Path::parent)];
        for dir in parents.into_iter().flatten() {
            if std::fs::read_dir(dir).is_ok_and(|mut d| d.next().is_none()) {
                let _ = std::fs::remove_dir(dir);
            }
        }
    }
    Ok(())
}

/// `are-hooks-installed`: all four forwarding scripts present and ours.
pub fn are_hooks_installed(repo: &Path) -> bool {
    HOOK_EVENTS.iter().all(|event| {
        std::fs::read_to_string(hook_script_path(repo, event))
            .is_ok_and(|content| content.contains("entire hooks cupel"))
    })
}

/// `parse-hook`: normalize a cupel hook payload into Entire's Event, or
/// `None` (-> literal `null`) for anything unrecognized - Entire treats
/// null as "nothing to record", not an error.
pub fn parse_hook(hook_name: &str, input: &[u8]) -> Option<Event> {
    if input.is_empty() {
        return None;
    }
    let payload: CupelHookPayload = serde_json::from_slice(input).ok()?;
    if payload.session_id.is_empty() {
        return None;
    }
    // The --hook name and the payload's own event field must agree; trust
    // the explicit flag (it is what Entire routed on).
    let kind = match hook_name {
        "session-start" => types::EVENT_SESSION_START,
        "user-prompt-submit" => types::EVENT_TURN_START,
        "stop" => types::EVENT_TURN_END,
        "session-end" => types::EVENT_SESSION_END,
        _ => return None,
    };
    // Best effort: the model id lives in the transcript header.
    let model = session::load_transcript(Path::new(&payload.session_ref))
        .map(|(header, _)| header.model)
        .unwrap_or_default();
    Some(Event {
        kind,
        session_id: payload.session_id,
        session_ref: payload.session_ref,
        prompt: payload.prompt,
        model,
        timestamp: types::rfc3339_utc(payload.timestamp),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use cupel_core::types::now_ms;

    fn temp_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("entire-agent-cupel-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Write a real transcript with cupel's own recorder, so these tests
    /// break if the shim ever disagrees with cupel's format.
    fn write_transcript(home: &Path, cwd: &Path) -> PathBuf {
        let mut recorder = cupel_coding_agent::session::SessionRecorder::new(
            Some(home.to_path_buf()),
            cwd,
            "cupel-77",
            "mock-model",
        );
        recorder.record(&AgentMessage::user_text("please fix main.rs"));
        let assistant = cupel_core::types::AssistantMessage {
            content: vec![
                AssistantContent::Text(cupel_core::types::TextContent::plain("on it")),
                AssistantContent::ToolCall(cupel_core::types::ToolCall {
                    id: "call_1".into(),
                    name: "edit".into(),
                    arguments: serde_json::json!({"path": "src/main.rs", "edits": []}),
                    thought_signature: None,
                }),
            ],
            api: cupel_core::types::Api::from("mock"),
            provider: cupel_core::types::Provider::from("mock"),
            model: "mock-model".into(),
            response_model: None,
            response_id: None,
            usage: cupel_core::types::Usage::default(),
            stop_reason: cupel_core::types::StopReason::Stop,
            error_message: None,
            timestamp: now_ms(),
        };
        recorder.record(&AgentMessage::Llm(Message::Assistant(assistant)));
        session::sessions_dir(Some(home), cwd)
            .unwrap()
            .join("cupel-77.jsonl")
    }

    #[test]
    fn info_declares_protocol_v1_and_hook_names() {
        let info = info();
        assert_eq!(info.protocol_version, 1);
        assert_eq!(info.name, "cupel");
        assert!(info.capabilities.hooks && info.capabilities.transcript_analyzer);
        assert_eq!(info.hook_names, HOOK_EVENTS.to_vec());
    }

    #[test]
    fn transcript_analysis_reads_cupel_written_transcripts() {
        let root = temp_root("analysis");
        let (home, cwd) = (root.join("home"), root.join("proj"));
        let transcript = write_transcript(&home, &cwd);

        assert_eq!(transcript_position(&transcript).unwrap(), 2);
        assert_eq!(transcript_position(Path::new("/nope.jsonl")).unwrap(), 0);

        let (files, position) = extract_modified_files(&transcript, 0).unwrap();
        assert_eq!(files, vec!["src/main.rs"]);
        assert_eq!(position, 2);
        // Offset past the tool call: nothing new.
        let (files, _) = extract_modified_files(&transcript, 2).unwrap();
        assert!(files.is_empty());

        assert_eq!(
            extract_prompts(&transcript, 0).unwrap(),
            vec!["please fix main.rs"]
        );
    }

    #[test]
    fn chunk_and_reassemble_roundtrip() {
        let content = b"0123456789abcdef".to_vec();
        let chunks = chunk_transcript(&content, 7).unwrap();
        assert_eq!(chunks.len(), 3); // 7 + 7 + 2 bytes
        assert_eq!(reassemble_transcript(&chunks).unwrap(), content);
        assert!(chunk_transcript(&content, 0).is_err());
    }

    #[test]
    fn read_and_write_session_roundtrip_the_transcript() {
        let root = temp_root("session");
        let (home, cwd) = (root.join("home"), root.join("proj"));
        let transcript = write_transcript(&home, &cwd);

        let input = HookInput {
            session_id: "cupel-77".into(),
            session_ref: transcript.display().to_string(),
        };
        let session = read_session(&input).unwrap();
        assert_eq!(session.session_id, "cupel-77");
        assert_eq!(session.agent_name, "cupel");
        assert_eq!(session.modified_files, vec!["src/main.rs"]);
        assert!(session.start_time.ends_with('Z'));

        // write-session restores the exact bytes elsewhere.
        let restored = root.join("restored.jsonl");
        let mut copy = session;
        copy.session_ref = restored.display().to_string();
        write_session(&copy).unwrap();
        assert_eq!(
            std::fs::read(&restored).unwrap(),
            std::fs::read(&transcript).unwrap()
        );
    }

    #[test]
    fn hooks_install_are_detected_and_uninstall_cleanly() {
        let repo = temp_root("hooks");
        assert!(!are_hooks_installed(&repo));

        assert_eq!(install_hooks(&repo, false).unwrap(), 4);
        assert!(are_hooks_installed(&repo));
        // Idempotent unless forced.
        assert_eq!(install_hooks(&repo, false).unwrap(), 0);
        assert_eq!(install_hooks(&repo, true).unwrap(), 4);
        let script = std::fs::read_to_string(repo.join(".cupel/hooks/stop/entire")).unwrap();
        assert!(script.contains("exec entire hooks cupel stop"));

        // A user hook in the same tree survives uninstall; ours vanish.
        std::fs::write(repo.join(".cupel/hooks/stop/mine.sh"), "#!/bin/sh\n").unwrap();
        uninstall_hooks(&repo).unwrap();
        assert!(!are_hooks_installed(&repo));
        assert!(repo.join(".cupel/hooks/stop/mine.sh").exists());
        assert!(!repo.join(".cupel/hooks/session-start").exists(), "pruned");
    }

    #[test]
    fn parse_hook_maps_events_and_rejects_garbage() {
        let payload = serde_json::json!({
            "event": "user-prompt-submit",
            "sessionId": "cupel-77",
            "sessionRef": "/nope.jsonl",
            "cwd": "/proj",
            "timestamp": 1_783_814_400_000_u64,
            "prompt": "fix it",
        })
        .to_string();

        let event = parse_hook("user-prompt-submit", payload.as_bytes()).unwrap();
        assert_eq!(event.kind, types::EVENT_TURN_START);
        assert_eq!(event.session_id, "cupel-77");
        assert_eq!(event.prompt, "fix it");
        assert_eq!(event.timestamp, "2026-07-12T00:00:00Z");

        assert_eq!(
            parse_hook("stop", payload.as_bytes()).unwrap().kind,
            types::EVENT_TURN_END
        );
        assert!(parse_hook("unknown-event", payload.as_bytes()).is_none());
        assert!(parse_hook("stop", b"").is_none());
        assert!(parse_hook("stop", b"not json").is_none());
        assert!(parse_hook("stop", b"{\"sessionId\":\"\"}").is_none());
    }

    #[test]
    fn resume_command_and_session_file_shapes() {
        assert_eq!(format_resume_command("cupel-9"), "cupel --resume cupel-9");
        assert_eq!(format_resume_command(""), "cupel --resume");
        assert_eq!(
            resolve_session_file(Path::new("/s"), "cupel-9"),
            PathBuf::from("/s/cupel-9.jsonl")
        );
    }
}
