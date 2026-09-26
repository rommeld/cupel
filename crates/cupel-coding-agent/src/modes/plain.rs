//! Plain mode: a line-based REPL on a TTY, or one prompt from piped stdin.
//!
//! Used when stdout is not a terminal (pipes, CI) or with `--plain`. It
//! prints unstyled text with no screen management so it can be captured.

use std::io::{IsTerminal as _, Write as _};

use futures_util::StreamExt as _;

use cupel_agent::{Agent, AgentEvent, AgentMessage};
use cupel_core::types::{AssistantMessageEvent, Message, ToolResultContent};

use crate::modes::SessionMeta;
use crate::session::SessionRecorder;

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

fn read_prompt(reader: &mut impl std::io::BufRead, piped: bool) -> Result<Option<String>, String> {
    let mut text = String::new();
    let bytes = if piped {
        reader.read_to_string(&mut text)
    } else {
        reader.read_line(&mut text)
    }
    .map_err(|error| error.to_string())?;
    Ok((bytes != 0).then_some(text))
}

pub async fn run(
    mut agent: Agent,
    meta: &SessionMeta,
    mut recorder: SessionRecorder,
) -> Result<(), String> {
    println!("cupel - {} ({})", meta.model_name, meta.provider);
    println!(
        "tools: read, grep, apply_patch, bash | cwd: {} | 'exit' to quit\n",
        meta.cwd
    );
    // Non-empty history at startup = a resumed session (seeded via
    // AgentOptions.messages in main).
    let restored = agent.state().messages.len();
    if restored > 0 {
        println!(
            "resumed session {} ({restored} messages)\n",
            recorder.session_id()
        );
    }

    let stdin = std::io::stdin();
    let piped = !stdin.is_terminal();
    let mut reader = stdin.lock();
    loop {
        if !piped {
            print!("> ");
            std::io::stdout().flush().ok();
        }

        let Some(line) = read_prompt(&mut reader, piped)? else {
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
                        println!("  /{}  - {}", c.name, c.description);
                    }
                    for t in meta
                        .templates
                        .iter()
                        .filter(|t| plain_command(&t.name) == PlainCommand::Prompt)
                    {
                        println!("  /{}  - {}", t.name, t.description);
                    }
                    println!();
                    continue;
                }
                // Same builder as the TUI; here the whole path is
                // synchronous — gather, then fall through to the ordinary
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
                            println!("{e}");
                            continue;
                        }
                    }
                }
                PlainCommand::TuiOnly => {
                    if matches!(name, "model" | "provider") {
                        println!("/{name} is TUI-only; use --model <id> at startup in plain mode");
                    } else {
                        println!("/{name} is TUI-only; use cupel in an interactive terminal");
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
        // directory (idempotent, never fails). Deferred to here — not
        // startup — so `cupel --plain < /dev/null` etc. leave no trace.
        crate::resources::ensure_project_dot_cupel(std::path::Path::new(&meta.cwd));
        // Transcript + hooks: creates the transcript lazily, settles any
        // pending stop hook, fires session-start (once) and
        // user-prompt-submit before the run begins.
        recorder.before_prompt(&prompt).await;

        let mut events = agent.prompt_text(&prompt).map_err(|e| e.to_string())?;

        // Render the event stream without terminal styling. Text deltas
        // print incrementally; tool calls appear as one-liners.
        let mut in_thinking = false;
        let mut thinking_newlines = 0;
        while let Some(event) = events.next().await {
            match event {
                // Every finalized message (user, assistant, tool result)
                // rides into the transcript; display still renders from the
                // streaming deltas below.
                AgentEvent::MessageEnd { message } => recorder.record(&message),
                AgentEvent::MessageUpdate { event } => match event {
                    AssistantMessageEvent::TextDelta { delta, .. } => {
                        if in_thinking {
                            print!("{}", reasoning_separator(thinking_newlines));
                            in_thinking = false;
                        }
                        print!("{delta}");
                        std::io::stdout().flush().ok();
                    }
                    AssistantMessageEvent::ThinkingDelta { delta, .. } => {
                        if !in_thinking {
                            thinking_newlines = 0;
                        }
                        thinking_newlines = reasoning_newlines(thinking_newlines, &delta);
                        in_thinking = true;
                        print!("{delta}");
                        std::io::stdout().flush().ok();
                    }
                    AssistantMessageEvent::ToolCallEnd { tool_call, .. } => {
                        if in_thinking {
                            print!("{}", reasoning_separator(thinking_newlines));
                            in_thinking = false;
                        } else {
                            println!();
                        }
                        println!(
                            "[{}]",
                            agent.describe_tool_call(&tool_call.name, &tool_call.arguments)
                        );
                    }
                    _ => {}
                },
                AgentEvent::ToolExecutionEnd {
                    result, is_error, ..
                } => {
                    if is_error {
                        print!("error: ");
                    }
                    if let Some(diff) = result
                        .details
                        .as_ref()
                        .and_then(|d| d.get("diff"))
                        .and_then(serde_json::Value::as_str)
                    {
                        for line in diff.lines() {
                            println!("{line}");
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
                        println!("{}", preview.join("\n"));
                        if more > 0 {
                            println!("... ({more} more lines)");
                        }
                    }
                }
                AgentEvent::TurnEnd { message, .. } => {
                    let ended_thinking = in_thinking;
                    if in_thinking {
                        print!("{}", reasoning_separator(thinking_newlines));
                        in_thinking = false;
                    }
                    if let AgentMessage::Llm(Message::Assistant(assistant)) = message.as_ref() {
                        let prefix = if ended_thinking { "" } else { "\n" };
                        if let Some(error) = &assistant.error_message {
                            println!("{prefix}error: {error}");
                        }
                        if assistant.stop_reason == cupel_core::types::StopReason::Length {
                            println!(
                                "{prefix}error: response was truncated before completion \
                                (output token limit)"
                            );
                        }
                        let usage = &assistant.usage;
                        println!(
                            "{prefix}[{} in / {} out / {} cached, ${:.4}]",
                            usage.input, usage.output, usage.cache_read, usage.cost.total
                        );
                    }
                }
                AgentEvent::CompactionStart { .. } => {
                    println!("compacting context...");
                }
                AgentEvent::CompactionEnd {
                    tokens_before,
                    tokens_after,
                    error,
                    summary,
                } => match error {
                    None => {
                        println!(
                            "context compacted: ~{}k -> ~{}k tokens",
                            tokens_before / 1000,
                            tokens_after / 1000
                        );
                        // The checkpoint the agent works from now.
                        if let Some(summary) = summary {
                            println!("{summary}\n");
                        }
                    }
                    Some(error) => println!("compaction failed: {error}"),
                },
                AgentEvent::AutoRetry {
                    attempt,
                    max_attempts,
                    delay_ms,
                    error_message,
                } => {
                    if in_thinking {
                        print!("{}", reasoning_separator(thinking_newlines));
                        in_thinking = false;
                    }
                    println!(
                        "retrying in {:.1}s (attempt {attempt}/{max_attempts}): \
                         {error_message}",
                        delay_ms as f64 / 1000.0
                    );
                }
                AgentEvent::ToolExecutionStart { .. } | AgentEvent::ToolExecutionUpdate { .. } => {}
                AgentEvent::AgentEnd { .. } => {
                    // Fire the `stop` hook without holding up the prompt
                    // loop; the next before_prompt settles it.
                    recorder.on_agent_end();
                    break;
                }
            }
        }
        agent.wait_for_idle().await;
        println!();
    }

    // Normal exit (EOF, `exit`, `/quit`): announce session-end to hooks.
    recorder.end_session().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::modes::plain::{
        PlainCommand, plain_command, read_prompt, reasoning_newlines, reasoning_separator,
    };

    #[test]
    fn piped_input_is_one_multiline_prompt_but_tty_input_is_line_based() {
        let mut piped = std::io::Cursor::new("first\nsecond\n");
        assert_eq!(
            read_prompt(&mut piped, true).unwrap().as_deref(),
            Some("first\nsecond\n")
        );
        assert!(read_prompt(&mut piped, true).unwrap().is_none());

        let mut tty = std::io::Cursor::new("first\nsecond\n");
        assert_eq!(
            read_prompt(&mut tty, false).unwrap().as_deref(),
            Some("first\n")
        );
        assert_eq!(
            read_prompt(&mut tty, false).unwrap().as_deref(),
            Some("second\n")
        );
        assert!(read_prompt(&mut tty, false).unwrap().is_none());
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
