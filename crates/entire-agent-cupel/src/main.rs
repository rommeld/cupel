//! `entire-agent-cupel` - the [Entire CLI](https://entire.io) external-agent
//! shim for cupel (protocol version 1).
//!
//! Entire discovers this binary by scanning $PATH for `entire-agent-<name>`
//! executables, validates it via `info`, and then drives it through
//! stateless subcommands that speak JSON over stdin/stdout: session/
//! transcript plumbing, hook installation and normalization, and transcript
//! analysis. Enable discovery per repo with `"external_agents": true` in
//! `.entire/settings.json`.
//!
//! The shim is intentionally thin: cupel already persists JSONL transcripts
//! and fires file-based lifecycle hooks (see cupel-coding-agent's session.rs
//! and hooks.rs); this binary translates between those and Entire's
//! protocol, reusing cupel's own code for every session-shaped question.

mod agent;
mod types;

use std::io::{Read as _, Write as _};
use std::path::PathBuf;

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(subcommand) = args.first() else {
        eprintln!("usage: entire-agent-cupel <subcommand> (Entire external-agent protocol v1)");
        return std::process::ExitCode::FAILURE;
    };
    match dispatch(subcommand, &args[1..]) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(message) => {
            // Protocol contract: non-zero exit + the error on stderr.
            eprintln!("{message}");
            std::process::ExitCode::FAILURE
        }
    }
}

/// `--name value` flag extraction; the protocol's flags are always this
/// simple pair shape, so no argument-parsing dependency is warranted.
fn flag_value(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

fn required_flag(args: &[String], name: &str) -> Result<String, String> {
    flag_value(args, name).ok_or_else(|| format!("{name} is required"))
}

/// Write one JSON response to stdout (newline-terminated, like Go's
/// json.Encoder which the reference agents use).
fn respond(value: &impl serde::Serialize) -> Result<(), String> {
    let json = serde_json::to_string(value).map_err(|e| e.to_string())?;
    println!("{json}");
    Ok(())
}

fn read_stdin() -> Result<Vec<u8>, String> {
    let mut buffer = Vec::new();
    std::io::stdin()
        .read_to_end(&mut buffer)
        .map_err(|e| e.to_string())?;
    Ok(buffer)
}

fn read_stdin_json<T: serde::de::DeserializeOwned>() -> Result<T, String> {
    serde_json::from_slice(&read_stdin()?).map_err(|e| format!("invalid JSON on stdin: {e}"))
}

fn dispatch(subcommand: &str, args: &[String]) -> Result<(), String> {
    match subcommand {
        // ---- identity ----------------------------------------------------
        "info" => respond(&agent::info()),
        "detect" => respond(&types::DetectResponse {
            present: agent::detect(),
        }),

        // ---- session plumbing --------------------------------------------
        "get-session-id" => {
            let input: types::HookInput = read_stdin_json()?;
            respond(&types::SessionIdResponse {
                session_id: input.session_id,
            })
        }
        "get-session-dir" => {
            let repo = required_flag(args, "--repo-path")?;
            respond(&types::SessionDirResponse {
                session_dir: agent::session_dir(&PathBuf::from(repo))?
                    .display()
                    .to_string(),
            })
        }
        "resolve-session-file" => {
            let dir = required_flag(args, "--session-dir")?;
            let id = required_flag(args, "--session-id")?;
            respond(&types::SessionFileResponse {
                session_file: agent::resolve_session_file(&PathBuf::from(dir), &id)
                    .display()
                    .to_string(),
            })
        }
        "read-session" => respond(&agent::read_session(&read_stdin_json()?)?),
        "write-session" => agent::write_session(&read_stdin_json()?),
        "format-resume-command" => {
            let id = flag_value(args, "--session-id").unwrap_or_default();
            respond(&types::ResumeCommandResponse {
                command: agent::format_resume_command(&id),
            })
        }

        // ---- transcript bytes --------------------------------------------
        "read-transcript" => {
            let path = required_flag(args, "--session-ref")?;
            let data = std::fs::read(&path).map_err(|e| format!("cannot read {path}: {e}"))?;
            // Raw bytes, NOT JSON - the one subcommand that streams content.
            std::io::stdout()
                .write_all(&data)
                .map_err(|e| e.to_string())
        }
        "chunk-transcript" => {
            let max_size: usize = required_flag(args, "--max-size")?
                .parse()
                .map_err(|e| format!("--max-size: {e}"))?;
            respond(&types::ChunkResponse {
                chunks: agent::chunk_transcript(&read_stdin()?, max_size)?,
            })
        }
        "reassemble-transcript" => {
            let input: types::ChunkResponse = read_stdin_json()?;
            let data = agent::reassemble_transcript(&input.chunks)?;
            std::io::stdout()
                .write_all(&data)
                .map_err(|e| e.to_string())
        }

        // ---- hooks ---------------------------------------------------------
        "parse-hook" => {
            let hook = required_flag(args, "--hook")?;
            match agent::parse_hook(&hook, &read_stdin()?) {
                Some(event) => respond(&event),
                // "Nothing to record" is a literal null, not an error.
                None => {
                    println!("null");
                    Ok(())
                }
            }
        }
        "install-hooks" => {
            let force = args.iter().any(|a| a == "--force");
            respond(&types::HooksInstalledCountResponse {
                hooks_installed: agent::install_hooks(&agent::repo_root(), force)?,
            })
        }
        "uninstall-hooks" => agent::uninstall_hooks(&agent::repo_root()),
        "are-hooks-installed" => respond(&types::AreHooksInstalledResponse {
            installed: agent::are_hooks_installed(&agent::repo_root()),
        }),

        // ---- transcript analysis -------------------------------------------
        "get-transcript-position" => {
            let path = required_flag(args, "--path")?;
            respond(&types::TranscriptPositionResponse {
                position: agent::transcript_position(&PathBuf::from(path))?,
            })
        }
        "extract-modified-files" => {
            let path = required_flag(args, "--path")?;
            let offset = parse_offset(args)?;
            let (files, current_position) =
                agent::extract_modified_files(&PathBuf::from(path), offset)?;
            respond(&types::ExtractFilesResponse {
                files,
                current_position,
            })
        }
        "extract-prompts" => {
            let path = required_flag(args, "--session-ref")?;
            let offset = parse_offset(args)?;
            respond(&types::ExtractPromptsResponse {
                prompts: agent::extract_prompts(&PathBuf::from(path), offset)?,
            })
        }
        // cupel keeps compaction summaries in the live context, not the
        // transcript, so there is never a stored summary to hand over.
        "extract-summary" => respond(&types::ExtractSummaryResponse {
            summary: String::new(),
            has_summary: false,
        }),

        other => Err(format!("unknown subcommand: {other}")),
    }
}

fn parse_offset(args: &[String]) -> Result<usize, String> {
    flag_value(args, "--offset").map_or(Ok(0), |v| v.parse().map_err(|e| format!("--offset: {e}")))
}
