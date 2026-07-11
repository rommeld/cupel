//! Plain mode: the line-based REPL (formerly the whole `main.rs`).
//!
//! Used when stdout is not a terminal (pipes, CI) or with `--plain`. It
//! prints raw text with a little ANSI color and no screen management -
//! exactly what you want when the output is being captured.

use std::io::Write as _;

use futures_util::StreamExt as _;

use cupel_agent::{Agent, AgentEvent, AgentMessage};
use cupel_core::types::{AssistantMessageEvent, Message, ToolResultContent};

use crate::modes::SessionMeta;

pub async fn run(mut agent: Agent, meta: &SessionMeta) -> Result<(), String> {
    println!("cupel - {} ({})", meta.model_name, meta.provider);
    println!(
        "tools: read, bash, edit, write, grep | cwd: {} | 'exit' to quit\n",
        meta.cwd
    );

    let stdin = std::io::stdin();
    loop {
        print!("> ");
        std::io::stdout().flush().ok();

        let mut line = String::new();
        if stdin.read_line(&mut line).map_err(|e| e.to_string())? == 0 {
            break; // EOF (Ctrl-D)
        }
        let input = line.trim();
        if input.is_empty() {
            continue;
        }
        if input == "exit" || input == "quit" {
            break;
        }

        // Slash commands: a minimal built-in set for plain mode (the TUI
        // has the full one), plus prompt-template expansion. Unknown
        // /commands pass through to the model as literal text.
        let mut prompt = input.to_string();
        if let Some(rest) = input.strip_prefix('/') {
            match rest
                .split_once(char::is_whitespace)
                .map_or(rest, |(n, _)| n)
            {
                "quit" => break,
                "help" => {
                    for c in crate::commands::BUILTIN_COMMANDS {
                        println!("  /{}  - {}", c.name, c.description);
                    }
                    for t in &meta.templates {
                        println!("  /{}  - {}", t.name, t.description);
                    }
                    println!();
                    continue;
                }
                _ => {
                    if let Some(expanded) =
                        crate::commands::expand_prompt_template(input, &meta.templates)
                    {
                        prompt = expanded;
                    }
                }
            }
        }

        let mut events = agent.prompt_text(&prompt).map_err(|e| e.to_string())?;

        // Render the event stream. Text deltas print incrementally; thinking
        // is dimmed; tool calls appear as one-liners.
        let mut in_thinking = false;
        while let Some(event) = events.next().await {
            match event {
                AgentEvent::MessageUpdate { event } => match event {
                    AssistantMessageEvent::TextDelta { delta, .. } => {
                        if in_thinking {
                            // Close the dim ANSI style from thinking output.
                            print!("\x1b[0m\n\n");
                            in_thinking = false;
                        }
                        print!("{delta}");
                        std::io::stdout().flush().ok();
                    }
                    AssistantMessageEvent::ThinkingDelta { delta, .. } => {
                        if !in_thinking {
                            print!("\x1b[2m"); // dim
                            in_thinking = true;
                        }
                        print!("{delta}");
                        std::io::stdout().flush().ok();
                    }
                    AssistantMessageEvent::ToolCallEnd { tool_call, .. } => {
                        if in_thinking {
                            print!("\x1b[0m\n\n");
                            in_thinking = false;
                        }
                        println!(
                            "\n\x1b[36m[{}] {}\x1b[0m",
                            tool_call.name, tool_call.arguments
                        );
                    }
                    _ => {}
                },
                AgentEvent::ToolExecutionEnd {
                    result, is_error, ..
                } => {
                    let text: String = result
                        .content
                        .iter()
                        .filter_map(|c| match c {
                            ToolResultContent::Text(t) => Some(t.text.as_str()),
                            ToolResultContent::Image(_) => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    // Show the model's evidence, capped for terminal sanity.
                    let preview: Vec<&str> = text.lines().take(10).collect();
                    let more = text.lines().count().saturating_sub(preview.len());
                    let style = if is_error { "\x1b[31m" } else { "\x1b[2m" };
                    println!("{style}{}\x1b[0m", preview.join("\n"));
                    if more > 0 {
                        println!("\x1b[2m... ({more} more lines)\x1b[0m");
                    }
                }
                AgentEvent::TurnEnd { message, .. } => {
                    if in_thinking {
                        print!("\x1b[0m\n\n");
                        in_thinking = false;
                    }
                    if let AgentMessage::Llm(Message::Assistant(assistant)) = message.as_ref() {
                        if let Some(error) = &assistant.error_message {
                            println!("\n\x1b[31merror: {error}\x1b[0m");
                        }
                        let usage = &assistant.usage;
                        println!(
                            "\n\x1b[2m[{} in / {} out / {} cached, ${:.4}]\x1b[0m",
                            usage.input, usage.output, usage.cache_read, usage.cost.total
                        );
                    }
                }
                AgentEvent::CompactionStart { .. } => {
                    println!("\x1b[33mcompacting context...\x1b[0m");
                }
                AgentEvent::CompactionEnd {
                    tokens_before,
                    tokens_after,
                    error,
                } => match error {
                    None => println!(
                        "\x1b[33mcontext compacted: ~{}k -> ~{}k tokens\x1b[0m",
                        tokens_before / 1000,
                        tokens_after / 1000
                    ),
                    Some(error) => println!("\x1b[31mcompaction failed: {error}\x1b[0m"),
                },
                AgentEvent::AutoRetry {
                    attempt,
                    max_attempts,
                    delay_ms,
                    error_message,
                } => {
                    if in_thinking {
                        print!("\x1b[0m\n\n");
                        in_thinking = false;
                    }
                    println!(
                        "\x1b[33mretrying in {:.1}s (attempt {attempt}/{max_attempts}): \
                         {error_message}\x1b[0m",
                        delay_ms as f64 / 1000.0
                    );
                }
                AgentEvent::AgentEnd { .. } => break,
                _ => {}
            }
        }
        agent.wait_for_idle().await;
        println!();
    }

    Ok(())
}
