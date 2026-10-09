//! Plain mode: a line-based REPL on a TTY, or one prompt from piped stdin.
//!
//! Used when stdout is not a terminal (pipes, CI) or with `--plain`. It
//! prints unstyled text with no screen management so it can be captured.

use std::io::{IsTerminal as _, Write as _};

use futures_util::StreamExt as _;
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _};

use cupel_agent::{Agent, AgentEvent, AgentEventStream, AgentMessage};
use cupel_core::types::{AssistantMessageEvent, Message, StopReason, ToolResultContent};

use crate::modes::SessionMeta;
#[cfg(unix)]
use crate::modes::TerminationSignals;
use crate::session::SessionRecorder;

enum PlainError {
    Run(String),
    Output(std::io::Error),
    #[cfg(unix)]
    Signal(i32),
}

fn write_output(
    mut writer: impl std::io::Write,
    args: std::fmt::Arguments<'_>,
) -> std::io::Result<()> {
    writer.write_fmt(args)?;
    writer.flush()
}

fn output(args: std::fmt::Arguments<'_>) -> Result<(), PlainError> {
    write_output(std::io::stdout().lock(), args).map_err(PlainError::Output)
}

#[derive(Debug, PartialEq, Eq)]
enum PlainCommand {
    Quit,
    Help,
    Review,
    TuiOnly,
    Prompt,
}

fn plain_command(name: &str) -> PlainCommand {
    match name {
        "quit" => PlainCommand::Quit,
        "help" => PlainCommand::Help,
        "review" => PlainCommand::Review,
        _ if crate::commands::BUILTIN_COMMANDS
            .iter()
            .any(|command| command.name == name) =>
        {
            PlainCommand::TuiOnly
        }
        _ => PlainCommand::Prompt,
    }
}

// Keep exactly one blank line between reasoning and the next output.
// Deltas may split the final newlines across multiple events.
fn reasoning_newlines(previous: usize, delta: &str) -> usize {
    if delta.bytes().all(|byte| byte == b'\n') {
        (previous + delta.len()).min(2)
    } else {
        delta
            .bytes()
            .rev()
            .take_while(|byte| *byte == b'\n')
            .count()
            .min(2)
    }
}

fn reasoning_separator(newlines: usize) -> &'static str {
    match newlines {
        0 => "\n\n",
        1 => "\n",
        _ => "",
    }
}

fn turn_failure(reason: StopReason, message: Option<&str>) -> Option<String> {
    let fallback = match reason {
        StopReason::Error => "model request failed",
        StopReason::Aborted => "model request aborted",
        _ => return None,
    };
    Some(
        message
            .filter(|text| !text.is_empty())
            .unwrap_or(fallback)
            .to_string(),
    )
}

async fn read_prompt(
    reader: &mut (impl tokio::io::AsyncBufRead + Unpin),
    piped: bool,
) -> Result<Option<String>, String> {
    let mut text = String::new();
    let bytes = if piped {
        reader.read_to_string(&mut text).await
    } else {
        reader.read_line(&mut text).await
    }
    .map_err(|error| error.to_string())?;
    Ok((bytes != 0).then_some(text))
}

pub async fn run(
    mut agent: Agent,
    meta: &SessionMeta,
    mut recorder: SessionRecorder,
) -> Result<(), String> {
    let mut events = None;
    let result = run_session(&mut agent, meta, &mut recorder, &mut events).await;
    if result.is_err() {
        agent.abort();
    }
    // Cancellation must finish killing detached tool process groups before
    // exit. Keep the stream so finalized messages are not lost on shutdown.
    agent.wait_for_idle().await;
    if let Some(mut events) = events {
        while let Some(event) = events.next().await {
            match event {
                AgentEvent::MessageEnd { message } => recorder.record(&message),
                AgentEvent::AgentEnd { .. } => recorder.on_agent_end(),
                _ => {}
            }
        }
    }
    recorder.end_session().await;

    match result {
        Ok(()) => Ok(()),
        Err(PlainError::Output(error)) if error.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        Err(PlainError::Output(error)) => {
            Err(format!("failed to write plain-mode output: {error}"))
        }
        Err(PlainError::Run(error)) => Err(error),
        #[cfg(unix)]
        Err(PlainError::Signal(signal)) => std::process::exit(128 + signal),
    }
}

async fn run_session(
    agent: &mut Agent,
    meta: &SessionMeta,
    recorder: &mut SessionRecorder,
    active_events: &mut Option<AgentEventStream>,
) -> Result<(), PlainError> {
    output(format_args!(
        "cupel - {} ({})\n",
        meta.model_name, meta.provider
    ))?;
    output(format_args!(
        "tools: read, grep, apply_patch, bash | cwd: {} | 'exit' to quit\n\n",
        meta.cwd
    ))?;
    // Non-empty history at startup = a resumed session (seeded via
    // AgentOptions.messages in main).
    let restored = agent.state().messages.len();
    if restored > 0 {
        output(format_args!(
            "resumed session {} ({restored} messages)\n\n",
            recorder.session_id()
        ))?;
    }

    #[cfg(unix)]
    let mut signals =
        TerminationSignals::new().map_err(|error| PlainError::Run(error.to_string()))?;
    let mut last_run_error = None;
    let piped = !std::io::stdin().is_terminal();
    let mut reader = tokio::io::BufReader::new(tokio::io::stdin());
    loop {
        if !piped {
            output(format_args!("> "))?;
        }

        #[cfg(unix)]
        let read = tokio::select! {
            biased;
            signal = signals.recv() => return Err(PlainError::Signal(signal)),
            read = read_prompt(&mut reader, piped) => read.map_err(PlainError::Run)?,
        };
        #[cfg(not(unix))]
        let read = read_prompt(&mut reader, piped)
            .await
            .map_err(PlainError::Run)?;
        let Some(line) = read else {
            break; // EOF (Ctrl-D on a TTY, or the end of a pipe)
        };
        let input = line.trim();
        if input.is_empty() {
            continue;
        }
        if input == "exit" || input == "quit" {
            break;
        }

        // Slash commands: only supported built-ins run locally. TUI-only
        // built-ins show a notice; unknown commands and prompt templates
        // retain their usual behavior.
        let mut prompt = input.to_string();
        if let Some(rest) = input.strip_prefix('/') {
            let name = rest
                .split_once(char::is_whitespace)
                .map_or(rest, |(n, _)| n);
            match plain_command(name) {
                PlainCommand::Quit => break,
                PlainCommand::Help => {
                    for c in crate::commands::BUILTIN_COMMANDS
                        .iter()
                        .filter(|c| plain_command(c.name) != PlainCommand::TuiOnly)
                    {
                        output(format_args!("  /{}  - {}\n", c.name, c.description))?;
                    }
                    for t in meta
                        .templates
                        .iter()
                        .filter(|t| plain_command(&t.name) == PlainCommand::Prompt)
                    {
                        output(format_args!("  /{}  - {}\n", t.name, t.description))?;
                    }
                    output(format_args!("\n"))?;
                    continue;
                }
                // Same builder as the TUI; here the whole path is
                // synchronous. Gather, then fall through to the ordinary
                // (blocking) prompt round-trip below.
                PlainCommand::Review => {
                    let review_args = crate::commands::parse_command_args(
                        rest.split_once(char::is_whitespace).map_or("", |(_, a)| a),
                    );
                    match crate::review::build_review_prompt(
                        std::path::Path::new(&meta.cwd),
                        &review_args,
                    ) {
                        Ok(built) => prompt = built,
                        Err(e) => {
                            output(format_args!("{e}\n"))?;
                            continue;
                        }
                    }
                }
                PlainCommand::TuiOnly => {
                    if matches!(name, "model" | "provider") {
                        output(format_args!(
                            "/{name} is TUI-only; use --model <id> at startup in plain mode\n"
                        ))?;
                    } else {
                        output(format_args!(
                            "/{name} is TUI-only; use cupel in an interactive terminal\n"
                        ))?;
                    }
                    continue;
                }
                PlainCommand::Prompt => {
                    if let Some(expanded) =
                        crate::commands::expand_prompt_template(input, &meta.templates)
                    {
                        prompt = expanded;
                    }
                }
            }
        }

        // First real agent interaction: scaffold the project .cupel/
        // directory (idempotent, never fails). Deferred until here rather
        // than startup so `cupel --plain < /dev/null` etc. leave no trace.
        crate::resources::ensure_project_dot_cupel(std::path::Path::new(&meta.cwd));
        // Transcript + hooks: creates the transcript lazily, settles any
        // pending stop hook, fires session-start (once) and
        // user-prompt-submit before the run begins.
        recorder.before_prompt(&prompt).await;

        *active_events = Some(
            agent
                .prompt_text(&prompt)
                .map_err(|e| PlainError::Run(e.to_string()))?,
        );
        let events = active_events.as_mut().expect("just started a run");
        // Retry/compaction can recover from an earlier failed turn. Only
        // the final turn of the last run determines the process exit code.
        last_run_error = None;

        // Render the event stream without terminal styling. Text deltas
        // print incrementally; tool calls appear as one-liners.
        let mut in_thinking = false;
        let mut thinking_newlines = 0;
        loop {
            #[cfg(unix)]
            let event = tokio::select! {
                biased;
                signal = signals.recv() => return Err(PlainError::Signal(signal)),
                event = events.next() => event,
            };
            #[cfg(not(unix))]
            let event = events.next().await;
            let Some(event) = event else { break };
            match event {
                // Every finalized message (user, assistant, tool result)
                // rides into the transcript; display still renders from the
                // streaming deltas below.
                AgentEvent::MessageEnd { message } => recorder.record(&message),
                AgentEvent::MessageUpdate { event } => match event {
                    AssistantMessageEvent::TextDelta { delta, .. } => {
                        if in_thinking {
                            output(format_args!("{}", reasoning_separator(thinking_newlines)))?;
                            in_thinking = false;
                        }
                        output(format_args!("{delta}"))?;
                    }
                    AssistantMessageEvent::ThinkingDelta { delta, .. } => {
                        if !in_thinking {
                            thinking_newlines = 0;
                        }
                        thinking_newlines = reasoning_newlines(thinking_newlines, &delta);
                        in_thinking = true;
                        output(format_args!("{delta}"))?;
                    }
                    AssistantMessageEvent::ToolCallEnd { tool_call, .. } => {
                        if in_thinking {
                            output(format_args!("{}", reasoning_separator(thinking_newlines)))?;
                            in_thinking = false;
                        } else {
                            output(format_args!("\n"))?;
                        }
                        output(format_args!(
                            "[{}]\n",
                            agent.describe_tool_call(&tool_call.name, &tool_call.arguments)
                        ))?;
                    }
                    _ => {}
                },
                AgentEvent::ToolExecutionEnd {
                    result, is_error, ..
                } => {
                    if is_error {
                        output(format_args!("error: "))?;
                    }
                    if let Some(diff) = result
                        .details
                        .as_ref()
                        .and_then(|d| d.get("diff"))
                        .and_then(serde_json::Value::as_str)
                    {
                        for line in diff.lines() {
                            output(format_args!("{line}\n"))?;
                        }
                    } else {
                        let text: String = result
                            .content
                            .iter()
                            .filter_map(|c| match c {
                                ToolResultContent::Text(t) => Some(t.text.as_str()),
                                ToolResultContent::Image(_) => None,
                            })
                            .collect::<Vec<_>>()
                            .join("\n");
                        let preview: Vec<&str> = text.lines().take(10).collect();
                        let more = text.lines().count().saturating_sub(preview.len());
                        output(format_args!("{}\n", preview.join("\n")))?;
                        if more > 0 {
                            output(format_args!("... ({more} more lines)\n"))?;
                        }
                    }
                }
                AgentEvent::TurnEnd { message, .. } => {
                    let ended_thinking = in_thinking;
                    if in_thinking {
                        output(format_args!("{}", reasoning_separator(thinking_newlines)))?;
                        in_thinking = false;
                    }
                    if let AgentMessage::Llm(Message::Assistant(assistant)) = message.as_ref() {
                        let prefix = if ended_thinking { "" } else { "\n" };
                        last_run_error =
                            turn_failure(assistant.stop_reason, assistant.error_message.as_deref());
                        if assistant.stop_reason == StopReason::Length {
                            let _ = writeln!(
                                std::io::stderr().lock(),
                                "error: response was truncated before completion \
                                (output token limit)"
                            );
                        }
                        let usage = &assistant.usage;
                        output(format_args!(
                            "{prefix}[{} in / {} out / {} cached, ${:.4}]\n",
                            usage.input, usage.output, usage.cache_read, usage.cost.total
                        ))?;
                    }
                }
                AgentEvent::CompactionStart { .. } => {
                    output(format_args!("compacting context...\n"))?;
                }
                AgentEvent::CompactionEnd {
                    tokens_before,
                    tokens_after,
                    error,
                    summary,
                    ..
                } => match error {
                    None => {
                        output(format_args!(
                            "context compacted: ~{}k -> ~{}k tokens\n",
                            tokens_before / 1000,
                            tokens_after / 1000
                        ))?;
                        // The checkpoint the agent works from now.
                        if let Some(summary) = summary {
                            output(format_args!("{summary}\n\n"))?;
                        }
                    }
                    Some(error) => output(format_args!("compaction failed: {error}\n"))?,
                },
                AgentEvent::AutoRetry {
                    attempt,
                    max_attempts,
                    delay_ms,
                    error_message,
                } => {
                    if in_thinking {
                        output(format_args!("{}", reasoning_separator(thinking_newlines)))?;
                        in_thinking = false;
                    }
                    output(format_args!(
                        "retrying in {:.1}s (attempt {attempt}/{max_attempts}): \
                         {error_message}\n",
                        delay_ms as f64 / 1000.0
                    ))?;
                }
                AgentEvent::ToolExecutionStart { .. }
                | AgentEvent::ToolExecutionUpdate { .. }
                | AgentEvent::ThinkingBlocksRemoved { .. } => {}
                AgentEvent::AgentEnd { .. } => {
                    // Fire the `stop` hook without holding up the prompt
                    // loop; the next before_prompt settles it.
                    recorder.on_agent_end();
                    break;
                }
            }
        }
        agent.wait_for_idle().await;
        output(format_args!("\n"))?;
    }

    // main formats errors on stderr and returns a nonzero exit status.
    last_run_error.map_or(Ok(()), |error| Err(PlainError::Run(error)))
}

#[cfg(test)]
mod tests {
    use crate::modes::plain::{
        PlainCommand, plain_command, read_prompt, reasoning_newlines, reasoning_separator,
        turn_failure, write_output,
    };
    use cupel_core::types::StopReason;

    struct FailingWriter {
        kind: std::io::ErrorKind,
        fail_on_flush: bool,
    }

    impl std::io::Write for FailingWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            if self.fail_on_flush {
                Ok(buf.len())
            } else {
                Err(self.kind.into())
            }
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Err(self.kind.into())
        }
    }

    #[test]
    fn output_preserves_text_and_reports_write_and_flush_errors() {
        let mut bytes = Vec::new();
        write_output(&mut bytes, format_args!("{}\n", "hello")).unwrap();
        assert_eq!(bytes, b"hello\n");
        for kind in [
            std::io::ErrorKind::BrokenPipe,
            std::io::ErrorKind::PermissionDenied,
        ] {
            for fail_on_flush in [false, true] {
                let writer = FailingWriter {
                    kind,
                    fail_on_flush,
                };
                assert_eq!(
                    write_output(writer, format_args!("hello"))
                        .unwrap_err()
                        .kind(),
                    kind
                );
            }
        }
    }

    #[test]
    fn only_terminal_model_failures_produce_an_error() {
        assert_eq!(
            turn_failure(StopReason::Error, Some("provider returned HTTP 400")),
            Some("provider returned HTTP 400".into())
        );
        assert_eq!(
            turn_failure(StopReason::Aborted, None),
            Some("model request aborted".into())
        );
        assert_eq!(
            turn_failure(StopReason::Error, None),
            Some("model request failed".into())
        );
        assert_eq!(
            turn_failure(StopReason::Error, Some("")),
            Some("model request failed".into())
        );
        // A later successful turn clears an error from an earlier retry.
        let mut last_error = turn_failure(StopReason::Error, Some("retryable"));
        assert!(last_error.is_some());
        last_error = turn_failure(StopReason::Stop, None);
        assert!(last_error.is_none());
        assert!(turn_failure(StopReason::ToolUse, None).is_none());
    }

    #[tokio::test]
    async fn piped_input_is_one_multiline_prompt_but_tty_input_is_line_based() {
        let mut piped = std::io::Cursor::new("first\nsecond\n");
        assert_eq!(
            read_prompt(&mut piped, true).await.unwrap().as_deref(),
            Some("first\nsecond\n")
        );
        assert!(read_prompt(&mut piped, true).await.unwrap().is_none());

        let mut tty = std::io::Cursor::new("first\nsecond\n");
        assert_eq!(
            read_prompt(&mut tty, false).await.unwrap().as_deref(),
            Some("first\n")
        );
        assert_eq!(
            read_prompt(&mut tty, false).await.unwrap().as_deref(),
            Some("second\n")
        );
        assert!(read_prompt(&mut tty, false).await.unwrap().is_none());
    }

    #[test]
    fn plain_help_only_advertises_supported_builtins() {
        let supported: Vec<_> = crate::commands::BUILTIN_COMMANDS
            .iter()
            .filter(|command| plain_command(command.name) != PlainCommand::TuiOnly)
            .map(|command| command.name)
            .collect();
        assert_eq!(supported, ["help", "quit", "review"]);
        for name in [
            "new",
            "model",
            "provider",
            "login",
            "logout",
            "thinking",
            "preset",
            "session-id",
            "hot-reload",
            "usage",
        ] {
            assert_eq!(plain_command(name), PlainCommand::TuiOnly, "{name}");
        }
        assert_eq!(plain_command("unknown"), PlainCommand::Prompt);
    }

    #[test]
    fn reasoning_spacing_handles_split_and_existing_newlines() {
        let mut newlines = reasoning_newlines(0, "summary\n");
        newlines = reasoning_newlines(newlines, "\n");
        assert_eq!(reasoning_separator(newlines), "");
        assert_eq!(
            reasoning_separator(reasoning_newlines(0, "summary\n")),
            "\n"
        );
        assert_eq!(
            reasoning_separator(reasoning_newlines(0, "summary")),
            "\n\n"
        );
        assert_eq!(reasoning_newlines(2, "more"), 0);
    }
}
