//! Runs `$SHELL -c <command>` in the agent's working directory, streaming
//! stdout+stderr into a bounded accumulator. The design constraints all come
//! from real agent behavior:
//!
//! - **Bounded memory, full fidelity.** A command can print gigabytes. The
//!   accumulator keeps only a rolling tail in memory (the model gets the
//!   LAST 2000 lines / 50 KB - errors live at the end), and spills the
//!   complete output to a temp file the moment limits are exceeded, so the
//!   truncation notice can say "Full output: /tmp/...".
//! - **Kill the whole tree.** `cargo test` spawns children; killing just the
//!   shell leaves them running. The child gets its own process group, and
//!   abort/timeout kills the group. Because this workspace forbids `unsafe`
//!   (no direct `libc::kill`), the group kill shells out to `kill -9 -PGID`
//!   - one extra process spawn on the rare abort path is a fine trade.
//! - **Exit codes are errors.** A non-zero exit becomes an error tool result
//!   (with the output attached) so the model *sees* failure as failure.

use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{Value, json};
use tokio::io::AsyncReadExt as _;
use tokio_util::sync::CancellationToken;

use cupel_agent::types::{AgentTool, AgentToolResult, ToolError, ToolUpdateFn};
use cupel_core::types::now_ms;

use crate::truncate::{
    DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, TruncatedBy, TruncationOptions, TruncationResult,
    format_size, truncate_tail,
};

/// Minimum interval between streamed progress updates to the UI.
const UPDATE_THROTTLE: Duration = Duration::from_millis(100);

#[derive(Debug, Deserialize)]
struct BashArgs {
    command: String,
    /// Timeout in seconds. No default: builds can legitimately take an hour.
    timeout: Option<f64>,
}

// ---------------------------------------------------------------------------
// Output accumulator
// ---------------------------------------------------------------------------

/// Streaming output tracker with bounded memory (see module docs).
struct OutputAccumulator {
    max_lines: usize,
    max_bytes: usize,
    /// In-memory rolling tail, trimmed to ~2x `max_bytes`.
    tail: Vec<u8>,
    /// Whether `tail` currently begins at a line boundary. After trimming it
    /// usually starts mid-line; snapshots drop that partial first line.
    tail_at_line_boundary: bool,
    total_bytes: usize,
    completed_lines: usize,
    has_open_line: bool,
    current_line_bytes: usize,
    /// Bytes received before the temp file was (maybe) opened.
    pending: Vec<u8>,
    temp: Option<(PathBuf, std::fs::File)>,
}

impl OutputAccumulator {
    fn new() -> Self {
        Self {
            max_lines: DEFAULT_MAX_LINES,
            max_bytes: DEFAULT_MAX_BYTES,
            tail: Vec::new(),
            tail_at_line_boundary: true,
            total_bytes: 0,
            completed_lines: 0,
            has_open_line: false,
            current_line_bytes: 0,
            pending: Vec::new(),
            temp: None,
        }
    }

    fn total_lines(&self) -> usize {
        self.completed_lines + usize::from(self.has_open_line)
    }

    fn append(&mut self, data: &[u8]) {
        self.total_bytes += data.len();

        // Line accounting: count newlines, track the open last line's size
        // (needed for the "line is 3MB" message when one line eats the budget).
        let mut last_newline = None;
        for (i, byte) in data.iter().enumerate() {
            if *byte == b'\n' {
                self.completed_lines += 1;
                last_newline = Some(i);
            }
        }
        match last_newline {
            None => {
                self.current_line_bytes += data.len();
                self.has_open_line = !data.is_empty() || self.has_open_line;
            }
            Some(i) => {
                self.current_line_bytes = data.len() - i - 1;
                self.has_open_line = self.current_line_bytes > 0;
            }
        }

        // Rolling tail with a trim threshold above the display budget so we
        // always have at least max_bytes of COMPLETE lines to show.
        self.tail.extend_from_slice(data);
        let rolling_cap = self.max_bytes * 2;
        if self.tail.len() > rolling_cap * 2 {
            let mut start = self.tail.len() - rolling_cap;
            // Never cut a UTF-8 character in half (continuation bytes are
            // 0b10xx_xxxx).
            while start < self.tail.len() && (self.tail[start] & 0xC0) == 0x80 {
                start += 1;
            }
            self.tail_at_line_boundary = start == 0 || self.tail[start - 1] == b'\n';
            self.tail.drain(..start);
        }

        // Full-output preservation: buffer in memory until limits trip, then
        // spill everything to a temp file and stream into it from then on.
        if let Some((_, file)) = &mut self.temp {
            use std::io::Write as _;
            let _ = file.write_all(data);
        } else {
            self.pending.extend_from_slice(data);
            if self.over_limits() {
                self.open_temp_file();
            }
        }
    }

    fn over_limits(&self) -> bool {
        self.total_bytes > self.max_bytes || self.total_lines() > self.max_lines
    }

    fn open_temp_file(&mut self) {
        if self.temp.is_some() {
            return;
        }
        let path = std::env::temp_dir().join(format!(
            "cupel-bash-{}-{}.log",
            std::process::id(),
            now_ms()
        ));
        if let Ok(mut file) = std::fs::File::create(&path) {
            use std::io::Write as _;
            let _ = file.write_all(&self.pending);
            self.pending = Vec::new();
            self.temp = Some((path, file));
        }
    }

    /// Current display view: tail-truncated text plus truncation metadata
    /// with TRUE totals (the tail alone under-counts what scrolled past).
    fn snapshot(&self) -> (String, TruncationResult, Option<PathBuf>) {
        let mut text = String::from_utf8_lossy(&self.tail).into_owned();
        if !self.tail_at_line_boundary
            && let Some(newline) = text.find('\n')
        {
            text = text[newline + 1..].to_string();
        }
        let mut truncation = truncate_tail(
            &text,
            TruncationOptions {
                max_lines: Some(self.max_lines),
                max_bytes: Some(self.max_bytes),
            },
        );
        // Overlay the real totals: the tail buffer only saw part of it all.
        truncation.truncated = self.over_limits();
        truncation.total_lines = self.total_lines();
        truncation.total_bytes = self.total_bytes;
        if truncation.truncated && truncation.truncated_by.is_none() {
            truncation.truncated_by = Some(if self.total_bytes > self.max_bytes {
                TruncatedBy::Bytes
            } else {
                TruncatedBy::Lines
            });
        }
        let content = truncation.content.clone();
        (
            content,
            truncation,
            self.temp.as_ref().map(|(p, _)| p.clone()),
        )
    }
}

// ---------------------------------------------------------------------------
// The tool
// ---------------------------------------------------------------------------

pub struct BashTool {
    cwd: PathBuf,
    description: String,
}

impl BashTool {
    #[must_use]
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            description: format!(
                "Execute a bash command in the current working directory. Returns stdout and \
                 stderr. Output is truncated to last {DEFAULT_MAX_LINES} lines or {}KB \
                 (whichever is hit first). If truncated, full output is saved to a temp file. \
                 Optionally provide a timeout in seconds.",
                DEFAULT_MAX_BYTES / 1024
            ),
        }
    }
}

/// SIGKILL the child's whole process group. Shells out to `kill` because
/// direct syscalls need `unsafe`, which this workspace forbids.
fn kill_process_group(pid: u32) {
    let _ = std::process::Command::new("kill")
        // `--` is required: a negative PID (= process group) looks like an
        // option flag otherwise. BSD kill (macOS) tolerates its absence,
        // Linux's procps kill does NOT - it silently refuses, the group
        // survives, and "timeout" waits out the full command (caught by CI
        // on the first-ever Linux run).
        .args(["-9", "--", &format!("-{pid}")])
        .output();
}

/// How a command run ended.
enum RunOutcome {
    Exited(Option<i32>),
    Aborted,
    TimedOut,
}

#[async_trait::async_trait]
impl AgentTool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {
                    "type": "string",
                    "description": "Bash command to execute"
                },
                "timeout": {
                    "type": "number",
                    "description": "Timeout in seconds (optional, no default timeout)"
                }
            },
            "required": ["command"]
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        args: Value,
        cancel: CancellationToken,
        on_update: Option<ToolUpdateFn>,
    ) -> Result<AgentToolResult, ToolError> {
        let args: BashArgs = serde_json::from_value(args)?;
        if let Some(timeout) = args.timeout
            && (!timeout.is_finite() || timeout <= 0.0)
        {
            return Err("Invalid timeout: must be a finite number of seconds".into());
        }
        if !self.cwd.exists() {
            return Err(format!(
                "Working directory does not exist: {}\nCannot execute bash commands.",
                self.cwd.display()
            )
            .into());
        }
        if cancel.is_cancelled() {
            return Err("Operation aborted".into());
        }

        // The user's shell, falling back to bash. `-c` runs one command
        // string, exactly how pi's shell config resolves on Unix.
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
        let mut command = tokio::process::Command::new(shell);
        command
            .arg("-c")
            .arg(&args.command)
            .current_dir(&self.cwd)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        // A fresh process group so abort/timeout can kill the whole tree.
        #[cfg(unix)]
        command.process_group(0);

        let mut child = command
            .spawn()
            .map_err(|e| format!("Failed to run shell: {e}"))?;
        let pid = child.id();

        // stdout and stderr are read on their own tasks feeding one channel,
        // preserving arrival order well enough for interleaved output (the
        // same guarantee pi gets from two `data` event listeners).
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        if let Some(stdout) = child.stdout.take() {
            spawn_reader(stdout, tx.clone());
        }
        if let Some(stderr) = child.stderr.take() {
            spawn_reader(stderr, tx);
        } else {
            // tx clones must all drop for rx to close; the stderr branch owns
            // the last one, so this path (no stderr) needs nothing extra.
        }

        let mut output = OutputAccumulator::new();
        // Backdated so the FIRST chunk sends an update immediately; falls
        // back to "now" on platforms where Instant can't go that far back.
        let mut last_update = std::time::Instant::now()
            .checked_sub(UPDATE_THROTTLE)
            .unwrap_or_else(std::time::Instant::now);
        let deadline = args
            .timeout
            .map(|secs| tokio::time::Instant::now() + Duration::from_secs_f64(secs));

        // Main loop: pump output chunks, racing cancellation and the timeout.
        let mut outcome: Option<RunOutcome> = None;
        while outcome.is_none() {
            tokio::select! {
                biased;
                () = cancel.cancelled() => {
                    if let Some(pid) = pid {
                        kill_process_group(pid);
                    }
                    // Also SIGKILL the direct child via tokio: even if the
                    // external `kill` misbehaves, the shell itself dies (and
                    // `bash -c` usually execs the command, so the shell IS
                    // the command).
                    let _ = child.start_kill();
                    outcome = Some(RunOutcome::Aborted);
                }
                () = async {
                    match deadline {
                        Some(deadline) => tokio::time::sleep_until(deadline).await,
                        None => std::future::pending().await,
                    }
                } => {
                    if let Some(pid) = pid {
                        kill_process_group(pid);
                    }
                    let _ = child.start_kill();
                    outcome = Some(RunOutcome::TimedOut);
                }
                chunk = rx.recv() => {
                    match chunk {
                        Some(chunk) => {
                            output.append(&chunk);
                            if let Some(on_update) = &on_update
                                && last_update.elapsed() >= UPDATE_THROTTLE
                            {
                                last_update = std::time::Instant::now();
                                let (content, _, _) = output.snapshot();
                                on_update(AgentToolResult::text(content));
                            }
                        }
                        // Both pipes closed: the command is done writing.
                        None => {
                            let status = child.wait().await?;
                            outcome = Some(RunOutcome::Exited(status.code()));
                        }
                    }
                }
            }
        }

        // After a kill, drain whatever was already in flight and reap the
        // child so it doesn't linger as a zombie.
        if !matches!(outcome, Some(RunOutcome::Exited(_))) {
            while let Ok(Some(chunk)) =
                tokio::time::timeout(Duration::from_millis(200), rx.recv()).await
            {
                output.append(&chunk);
            }
            // Bounded reap: if the process somehow survived both kills, give
            // up after a beat instead of hanging the turn - the OS reaps the
            // zombie when cupel exits, which beats a frozen agent.
            let _ = tokio::time::timeout(Duration::from_secs(2), child.wait()).await;
        }

        // ---- Format the final result -----------------------------------------
        let (content, truncation, full_output_path) = output.snapshot();
        let mut text = if content.is_empty() {
            String::new()
        } else {
            content
        };
        let mut details: Option<Value> = None;

        if truncation.truncated {
            let start_line = truncation.total_lines - truncation.output_lines + 1;
            let end_line = truncation.total_lines;
            let path_note = full_output_path
                .as_ref()
                .map_or_else(String::new, |p| format!(" Full output: {}", p.display()));
            let notice = if truncation.last_line_partial {
                format!(
                    "[Showing last {} of line {end_line} (line is {}).{path_note}]",
                    format_size(truncation.output_bytes),
                    format_size(output.current_line_bytes),
                )
            } else if truncation.truncated_by == Some(TruncatedBy::Lines) {
                format!(
                    "[Showing lines {start_line}-{end_line} of {}.{path_note}]",
                    truncation.total_lines
                )
            } else {
                format!(
                    "[Showing lines {start_line}-{end_line} of {} ({} limit).{path_note}]",
                    truncation.total_lines,
                    format_size(DEFAULT_MAX_BYTES),
                )
            };
            text = if text.is_empty() {
                notice
            } else {
                format!("{text}\n\n{notice}")
            };
            details = Some(json!({
                "truncated": true,
                "totalLines": truncation.total_lines,
                "fullOutputPath": full_output_path.map(|p| p.display().to_string()),
            }));
        }

        // Failure paths return Err so the loop marks the result as an error.
        let append_status = |text: &str, status: &str| {
            if text.is_empty() {
                status.to_string()
            } else {
                format!("{text}\n\n{status}")
            }
        };
        match outcome {
            Some(RunOutcome::Aborted) => Err(append_status(&text, "Command aborted").into()),
            Some(RunOutcome::TimedOut) => Err(append_status(
                &text,
                &format!(
                    "Command timed out after {} seconds",
                    args.timeout.unwrap_or(0.0)
                ),
            )
            .into()),
            Some(RunOutcome::Exited(code)) if code != Some(0) => {
                let code_text =
                    code.map_or_else(|| "killed by signal".to_string(), |c| c.to_string());
                Err(append_status(&text, &format!("Command exited with code {code_text}")).into())
            }
            _ => Ok(AgentToolResult {
                content: vec![cupel_core::types::ToolResultContent::Text(
                    cupel_core::types::TextContent::plain(if text.is_empty() {
                        "(no output)".to_string()
                    } else {
                        text
                    }),
                )],
                details,
                terminate: false,
            }),
        }
    }
}

/// Copy one pipe into the chunk channel until EOF.
fn spawn_reader(
    mut pipe: impl tokio::io::AsyncRead + Unpin + Send + 'static,
    tx: tokio::sync::mpsc::Sender<Vec<u8>>,
) {
    tokio::spawn(async move {
        let mut buffer = [0_u8; 8192];
        loop {
            match pipe.read(&mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if tx.send(buffer[..n].to_vec()).await.is_err() {
                        break; // Receiver gone: the run was torn down.
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use cupel_core::types::ToolResultContent;

    fn tool() -> BashTool {
        BashTool::new(std::env::temp_dir())
    }

    async fn run(args: Value) -> Result<String, String> {
        let result = tool()
            .execute("c", args, CancellationToken::new(), None)
            .await
            .map_err(|e| e.to_string())?;
        let ToolResultContent::Text(text) = &result.content[0] else {
            return Err("expected text".to_string());
        };
        Ok(text.text.clone())
    }

    #[tokio::test]
    async fn captures_stdout_and_stderr() {
        let out = run(json!({"command": "echo out; echo err >&2"}))
            .await
            .unwrap();
        assert!(out.contains("out"), "got: {out}");
        assert!(out.contains("err"), "got: {out}");
    }

    #[tokio::test]
    async fn nonzero_exit_is_an_error_with_output() {
        let err = run(json!({"command": "echo boom; exit 3"}))
            .await
            .unwrap_err();
        assert!(err.contains("boom"), "got: {err}");
        assert!(err.contains("exited with code 3"), "got: {err}");
    }

    #[tokio::test]
    async fn empty_output_reports_no_output() {
        let out = run(json!({"command": "true"})).await.unwrap();
        assert_eq!(out, "(no output)");
    }

    #[tokio::test]
    async fn timeout_kills_the_command() {
        let started = std::time::Instant::now();
        let err = run(json!({"command": "sleep 5", "timeout": 0.2}))
            .await
            .unwrap_err();
        assert!(err.contains("timed out"), "got: {err}");
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "kill was not prompt"
        );
    }

    #[tokio::test]
    async fn abort_kills_the_command() {
        let cancel = CancellationToken::new();
        let cancel_clone = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            cancel_clone.cancel();
        });
        let err = tool()
            .execute("c", json!({"command": "sleep 5"}), cancel, None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("aborted"), "got: {err}");
    }

    #[tokio::test]
    async fn long_output_truncates_to_tail_and_saves_full_output() {
        // 3000 lines exceeds the 2000-line limit; the model sees the tail.
        let err_or_ok = run(json!({"command": "seq 1 3000"})).await.unwrap();
        assert!(err_or_ok.contains("3000"), "tail should include the end");
        assert!(!err_or_ok.contains("\n1\n"), "head should be gone");
        assert!(err_or_ok.contains("Full output:"), "got: {err_or_ok}");
        assert!(err_or_ok.contains("of 3000"), "got: {err_or_ok}");
    }
}
