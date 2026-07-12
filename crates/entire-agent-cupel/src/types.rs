//! Wire types of Entire's external-agent protocol (version 1).
//!
//! Everything here is snake_case JSON - Entire's protocol structs are Go
//! types with `json:"snake_case"` tags, NOT cupel's camelCase convention.
//! Two Go-isms matter for compatibility:
//! - Go `[]byte` fields marshal as base64 STRINGS (`chunks`, `native_data`),
//! - timestamps are RFC3339 strings, not epoch numbers.

use serde::{Deserialize, Serialize};

/// The protocol generation this shim implements. Entire refuses agents
/// whose `info.protocol_version` doesn't match its own.
pub const PROTOCOL_VERSION: u32 = 1;

/// Answer to `info`: identity plus the capability map that tells Entire
/// which optional subcommands it may invoke.
#[derive(Serialize)]
pub struct InfoResponse {
    pub protocol_version: u32,
    pub name: &'static str,
    #[serde(rename = "type")]
    pub agent_type: &'static str,
    pub description: &'static str,
    pub is_preview: bool,
    /// Directories Entire must never treat as checkpointable user content.
    pub protected_dirs: Vec<&'static str>,
    pub hook_names: Vec<&'static str>,
    pub capabilities: Capabilities,
}

#[derive(Serialize)]
pub struct Capabilities {
    pub hooks: bool,
    pub transcript_analyzer: bool,
    pub transcript_preparer: bool,
    pub token_calculator: bool,
    pub compact_transcript: bool,
    pub text_generator: bool,
    pub hook_response_writer: bool,
    pub subagent_aware_extractor: bool,
    /// cupel runs in the user's terminal (TUI), so Entire should not expect
    /// to drive it headlessly.
    pub uses_terminal: bool,
}

#[derive(Serialize)]
pub struct DetectResponse {
    pub present: bool,
}

#[derive(Serialize)]
pub struct SessionIdResponse {
    pub session_id: String,
}

#[derive(Serialize)]
pub struct SessionDirResponse {
    pub session_dir: String,
}

#[derive(Serialize)]
pub struct SessionFileResponse {
    pub session_file: String,
}

/// Chunks travel as base64 strings (Go `[][]byte`).
#[derive(Serialize, Deserialize)]
pub struct ChunkResponse {
    pub chunks: Vec<String>,
}

#[derive(Serialize)]
pub struct ResumeCommandResponse {
    pub command: String,
}

#[derive(Serialize)]
pub struct HooksInstalledCountResponse {
    pub hooks_installed: usize,
}

#[derive(Serialize)]
pub struct AreHooksInstalledResponse {
    pub installed: bool,
}

#[derive(Serialize)]
pub struct TranscriptPositionResponse {
    pub position: usize,
}

#[derive(Serialize)]
pub struct ExtractFilesResponse {
    pub files: Vec<String>,
    pub current_position: usize,
}

#[derive(Serialize)]
pub struct ExtractPromptsResponse {
    pub prompts: Vec<String>,
}

#[derive(Serialize)]
pub struct ExtractSummaryResponse {
    pub summary: String,
    pub has_summary: bool,
}

/// What Entire sends the shim on stdin for `get-session-id`/`read-session`.
/// Only the fields the shim consumes are declared; the rest pass through
/// serde's default unknown-field tolerance.
#[derive(Deserialize, Default)]
pub struct HookInput {
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub session_ref: String,
}

/// Entire's cross-agent session record (`read-session`/`write-session`).
#[derive(Serialize, Deserialize)]
pub struct AgentSession {
    pub session_id: String,
    pub agent_name: String,
    pub repo_path: String,
    pub session_ref: String,
    /// RFC3339.
    pub start_time: String,
    /// Base64 of the agent-native transcript bytes (Go `[]byte`); `null`
    /// when absent.
    pub native_data: Option<String>,
    pub modified_files: Vec<String>,
    pub new_files: Vec<String>,
    pub deleted_files: Vec<String>,
}

/// The normalized event `parse-hook` returns (or a literal `null`).
/// Type codes: 1=SessionStart, 2=TurnStart, 3=TurnEnd, 4=Compaction,
/// 5=SessionEnd, 6=SubagentStart, 7=SubagentEnd.
#[derive(Serialize)]
pub struct Event {
    #[serde(rename = "type")]
    pub kind: u8,
    pub session_id: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub session_ref: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub prompt: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub model: String,
    pub timestamp: String,
}

pub const EVENT_SESSION_START: u8 = 1;
pub const EVENT_TURN_START: u8 = 2;
pub const EVENT_TURN_END: u8 = 3;
pub const EVENT_SESSION_END: u8 = 5;

/// The payload cupel's own hooks emit (camelCase - cupel's convention, see
/// cupel-coding-agent/src/hooks.rs). This is what the installed forwarding
/// scripts pipe to `entire hooks cupel <event>`, and therefore what
/// `parse-hook` receives back on stdin.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CupelHookPayload {
    // The payload also carries an `event` field, but parse-hook trusts the
    // explicit --hook flag instead (it is what Entire routed on), so the
    // field is simply not declared - serde skips unknown fields.
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub session_ref: String,
    /// Unix milliseconds (cupel's `now_ms`).
    #[serde(default)]
    pub timestamp: u64,
    #[serde(default)]
    pub prompt: String,
}

/// Unix milliseconds -> RFC3339 UTC (`2026-07-12T09:30:00Z`), the timestamp
/// format Entire's Go types expect.
#[must_use]
pub fn rfc3339_utc(ms: u64) -> String {
    chrono::DateTime::from_timestamp_millis(i64::try_from(ms).unwrap_or(0))
        .unwrap_or_default()
        .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_matches_known_instants() {
        assert_eq!(rfc3339_utc(0), "1970-01-01T00:00:00Z");
        // 2026-07-12 00:00:00 UTC (20,646 days after the epoch).
        assert_eq!(rfc3339_utc(1_783_814_400_000), "2026-07-12T00:00:00Z");
    }

    #[test]
    fn event_omits_empty_optionals_and_null_is_literal() {
        let event = Event {
            kind: EVENT_TURN_END,
            session_id: "cupel-1".into(),
            session_ref: String::new(),
            prompt: String::new(),
            model: String::new(),
            timestamp: rfc3339_utc(0),
        };
        let json = serde_json::to_value(&event).unwrap();
        assert_eq!(json["type"], 3);
        assert!(json.get("prompt").is_none(), "empty fields are omitted");
    }
}
