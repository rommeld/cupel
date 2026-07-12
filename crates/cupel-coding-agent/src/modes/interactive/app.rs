//! Application state and event handling.
//!
//! The TUI is a classic reducer: two event sources (terminal input, agent
//! events) mutate one `App`, and the render pass in `ui.rs` draws whatever
//! the `App` currently says. No state lives in the widgets - that's the
//! immediate-mode contract that keeps ratatui apps easy to reason about.

use futures_util::StreamExt as _;

use cupel_agent::{Agent, AgentEvent, AgentEventStream, AgentMessage};
use cupel_core::types::{
    AssistantContent, AssistantMessageEvent, Message, StopReason, ToolResultContent,
    UserContentBody,
};
use ratatui::crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEventKind,
};

use super::autocomplete::{Autocomplete, Candidate};
use super::input::InputState;
use super::transcript::{Cell, ToolOutcome, Transcript};
use crate::commands;
use crate::modes::SessionMeta;
use crate::session::SessionRecorder;

/// Cumulative token/cost counters across the whole session.
#[derive(Default)]
pub struct Totals {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cost: f64,
}

pub struct App {
    pub agent: Agent,
    pub meta: SessionMeta,
    pub transcript: Transcript,
    pub input: InputState,
    /// The `@path` file-reference popup (see `autocomplete.rs`).
    pub autocomplete: Autocomplete,
    pub totals: Totals,
    /// Event stream of the active run; `None` when idle.
    pub run_events: Option<AgentEventStream>,
    /// Scroll position measured in lines from the BOTTOM. 0 = follow output.
    /// Bottom-anchored (instead of top-anchored) means new output never
    /// yanks the view while the user is reading history.
    pub scroll_from_bottom: usize,
    /// Set by the render pass each frame so scrolling can clamp correctly.
    pub last_transcript_height: u16,
    pub last_total_lines: usize,
    pub should_quit: bool,
    /// Transcript writer + lifecycle-hook dispatcher for this session.
    pub recorder: SessionRecorder,
    /// A prompt accepted by `send()` but not yet started. The async event
    /// loop picks it up and awaits the prompt-path hooks first - key
    /// handling stays fully synchronous (and so do its tests).
    pub pending_prompt: Option<String>,
    /// Whether the terminal currently reports mouse events to cupel. ON =
    /// wheel scrolls the transcript; OFF ("selection mode") = the terminal
    /// handles the mouse natively, so text can be selected and copied.
    pub mouse_captured: bool,
    /// Set by the Ctrl+Y key handler; the event loop applies it (only the
    /// loop owns the terminal and can issue the crossterm commands).
    pub mouse_toggle_requested: bool,
}

impl App {
    #[must_use]
    pub fn new(agent: Agent, meta: SessionMeta, recorder: SessionRecorder) -> Self {
        // The /command autocomplete catalog: built-ins and prompt
        // templates, each labeled with its description.
        let mut command_candidates: Vec<Candidate> = commands::BUILTIN_COMMANDS
            .iter()
            .map(|c| Candidate {
                display: format!("{}  - {}", c.name, c.description),
                value: c.name.to_string(),
                is_dir: false,
            })
            .collect();
        for template in &meta.templates {
            command_candidates.push(Candidate {
                display: format!("{}  - {}", template.name, template.description),
                value: template.name.clone(),
                is_dir: false,
            });
        }
        // Argument value sets: after `/model ` the popup offers the catalog,
        // after `/thinking ` the levels - no more typing ids from memory.
        let model_candidates: Vec<Candidate> = cupel_core::catalog::builtin_models()
            .iter()
            .map(|model| Candidate {
                display: format!("{}  ({})", model.id, model.provider.as_str()),
                value: model.id.clone(),
                is_dir: false,
            })
            .collect();
        let thinking_candidates: Vec<Candidate> = [
            ("off", "no extended thinking"),
            ("minimal", "shortest thinking budget"),
            ("low", "small thinking budget"),
            ("medium", "moderate thinking budget"),
            ("high", "large thinking budget"),
            ("xhigh", "maximum thinking budget"),
        ]
        .iter()
        .map(|(level, description)| Candidate {
            display: format!("{level}  - {description}"),
            value: (*level).to_string(),
            is_dir: false,
        })
        .collect();
        let autocomplete = Autocomplete::new(&meta.cwd)
            .with_commands(command_candidates)
            .with_command_args("model", model_candidates)
            .with_command_args("thinking", thinking_candidates);
        // Restored history (a --resume session) exists before the App does;
        // snapshot it so the transcript can replay it as cells.
        let history = agent.state().messages;
        let mut app = Self {
            agent,
            meta,
            transcript: Transcript::default(),
            input: InputState::default(),
            autocomplete,
            totals: Totals::default(),
            run_events: None,
            scroll_from_bottom: 0,
            last_transcript_height: 0,
            last_total_lines: 0,
            should_quit: false,
            recorder,
            pending_prompt: None,
            mouse_captured: true, // mod.rs enables capture at startup
            mouse_toggle_requested: false,
        };
        app.replay_history(&history);
        app
    }

    /// Rebuild transcript cells from restored history so a resumed session
    /// looks like it never ended. Mirrors the live `MessageEnd` /
    /// `ToolExecutionEnd` handling in `on_agent_event`, but reads finalized
    /// messages instead of streaming events.
    fn replay_history(&mut self, messages: &[AgentMessage]) {
        if messages.is_empty() {
            return;
        }
        self.transcript.cells.push(Cell::Notice {
            text: format!(
                "resumed session {} ({} messages)",
                self.recorder.session_id(),
                messages.len()
            ),
        });
        for message in messages {
            match message {
                AgentMessage::Llm(Message::User(user)) => {
                    let text = match &user.content {
                        UserContentBody::Text(text) => text.clone(),
                        UserContentBody::Blocks(_) => "(rich message)".to_string(),
                    };
                    self.transcript.cells.push(Cell::User { text });
                }
                AgentMessage::Llm(Message::Assistant(assistant)) => {
                    for content in &assistant.content {
                        let cell = match content {
                            AssistantContent::Thinking(t) => Cell::Thinking {
                                text: t.thinking.clone(),
                            },
                            AssistantContent::Text(t) => Cell::Assistant {
                                text: t.text.clone(),
                            },
                            AssistantContent::ToolCall(call) => Cell::Tool {
                                id: call.id.clone(),
                                name: call.name.clone(),
                                args: compact(&call.arguments.to_string(), 200),
                                result: None,
                            },
                        };
                        self.transcript.cells.push(cell);
                    }
                    // Same error/usage bookkeeping as the live path, so
                    // /usage stays truthful across a resume.
                    if matches!(
                        assistant.stop_reason,
                        StopReason::Error | StopReason::Aborted
                    ) {
                        self.transcript.cells.push(Cell::Error {
                            text: assistant
                                .error_message
                                .clone()
                                .unwrap_or_else(|| "unknown error".to_string()),
                        });
                    } else {
                        let usage = &assistant.usage;
                        self.totals.input += usage.input;
                        self.totals.output += usage.output;
                        self.totals.cache_read += usage.cache_read;
                        self.totals.cost += usage.cost.total;
                        self.transcript.cells.push(Cell::Usage {
                            text: format!(
                                "[{} in / {} out / {} cached, ${:.4}]",
                                usage.input, usage.output, usage.cache_read, usage.cost.total
                            ),
                        });
                    }
                }
                AgentMessage::Llm(Message::ToolResult(result)) => {
                    let text: String = result
                        .content
                        .iter()
                        .filter_map(|c| match c {
                            ToolResultContent::Text(t) => Some(t.text.as_str()),
                            ToolResultContent::Image(_) => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    self.transcript.attach_tool_result(
                        &result.tool_call_id,
                        ToolOutcome {
                            text,
                            is_error: result.is_error,
                        },
                    );
                }
                // Custom messages are internal bookkeeping, not display.
                AgentMessage::Custom { .. } => {}
            }
        }
    }

    #[must_use]
    pub fn is_running(&self) -> bool {
        self.run_events.is_some()
    }

    // -----------------------------------------------------------------------
    // Terminal events
    // -----------------------------------------------------------------------

    pub fn on_terminal_event(&mut self, event: Event) {
        match event {
            // Windows terminals report key releases too; only act on press.
            Event::Key(key) if key.kind == KeyEventKind::Press => self.on_key(key),
            Event::Paste(text) => {
                self.input.insert_str(&text);
                self.autocomplete
                    .refresh(self.input.text(), self.input.cursor());
            }
            // The wheel scrolls the transcript wherever the pointer is -
            // same clamped movement as PgUp/PgDn, at the conventional
            // 3-lines-per-notch step. (Mouse events only arrive because
            // mod.rs enables mouse capture.)
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::ScrollUp => self.scroll_by(3),
                MouseEventKind::ScrollDown => self.scroll_by(-3),
                _ => {}
            },
            // Resize is handled implicitly: the next draw uses the new size.
            _ => {}
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);

        // ---- autocomplete popup takes precedence while open -----------------
        // It consumes ONLY the keys it needs; everything else (Ctrl-C
        // included) falls through so session control never changes meaning.
        if self.autocomplete.is_open() {
            match (key.code, ctrl, alt) {
                // Esc closes the popup - it does NOT abort the run. A second
                // Esc (popup now closed) aborts as usual.
                (KeyCode::Esc, ..) => {
                    self.autocomplete.close();
                    return;
                }
                // Up/Down move the selection, NOT the prompt history.
                (KeyCode::Up, ..) => {
                    self.autocomplete.move_up();
                    return;
                }
                (KeyCode::Down, ..) => {
                    self.autocomplete.move_down();
                    return;
                }
                // Tab or Enter accept - Enter does NOT submit while open.
                (KeyCode::Tab, ..) | (KeyCode::Enter, false, false) => {
                    if let Some(completion) = self.autocomplete.accept(self.input.cursor()) {
                        self.input.replace_range(
                            completion.start,
                            completion.end,
                            &completion.insert,
                        );
                    }
                    // Refresh decides what happens next: a completed FILE no
                    // longer forms a token (trailing space) so the popup
                    // closes; a DIRECTORY still does, so it keeps completing.
                    self.autocomplete
                        .refresh(self.input.text(), self.input.cursor());
                    return;
                }
                _ => {}
            }
        }

        match (key.code, ctrl, alt) {
            // ---- session control -----------------------------------------
            (KeyCode::Char('c'), true, _) => {
                if self.is_running() {
                    // First Ctrl-C aborts the run; when idle it quits.
                    self.agent.abort();
                } else {
                    self.should_quit = true;
                }
            }
            (KeyCode::Char('d'), true, _) if self.input.is_empty() && !self.is_running() => {
                self.should_quit = true;
            }
            (KeyCode::Esc, ..) if self.is_running() => self.agent.abort(),

            // ---- view control --------------------------------------------
            // Ctrl+Y toggles "selection mode": mouse capture off so the
            // TERMINAL owns the mouse again (select + copy text natively),
            // then back on for wheel scrolling. Only requested here - the
            // event loop owns the terminal and issues the actual commands.
            (KeyCode::Char('y'), true, _) => self.mouse_toggle_requested = true,
            (KeyCode::PageUp, ..) => {
                self.scroll_by(i64::from(self.last_transcript_height / 2).max(1));
            }
            (KeyCode::PageDown, ..) => {
                self.scroll_by(-i64::from(self.last_transcript_height / 2).max(-1));
            }

            // ---- editing --------------------------------------------------
            // Alt+Enter inserts a newline (Shift+Enter is indistinguishable
            // from Enter in most terminals, so Alt is the portable choice).
            (KeyCode::Enter, _, true) => {
                self.input.insert('\n');
                self.refresh_autocomplete();
            }
            (KeyCode::Enter, ..) => self.submit(),
            (KeyCode::Backspace, ..) => {
                self.input.delete_back();
                self.refresh_autocomplete();
            }
            (KeyCode::Delete, ..) => {
                self.input.delete_forward();
                self.refresh_autocomplete();
            }
            (KeyCode::Left, ..) => {
                self.input.move_left();
                self.refresh_autocomplete_if_open();
            }
            (KeyCode::Right, ..) => {
                self.input.move_right();
                self.refresh_autocomplete_if_open();
            }
            (KeyCode::Home, ..) | (KeyCode::Char('a'), true, _) => {
                self.input.move_home();
                self.refresh_autocomplete_if_open();
            }
            (KeyCode::End, ..) | (KeyCode::Char('e'), true, _) => {
                self.input.move_end();
                self.refresh_autocomplete_if_open();
            }
            // History recall closes the popup instead of refreshing: a
            // recalled prompt containing `@src/x` must not surprise-open the
            // menu. Completion triggers on TYPING (pi behaves the same).
            (KeyCode::Up, ..) => {
                self.input.history_prev();
                self.autocomplete.close();
            }
            (KeyCode::Down, ..) => {
                self.input.history_next();
                self.autocomplete.close();
            }
            (KeyCode::Char(c), false, false) => {
                self.input.insert(c);
                self.refresh_autocomplete();
            }
            _ => {}
        }
    }

    /// Edits (typing, deleting, pasting) re-evaluate the popup and may OPEN
    /// a session - completion is typing-driven.
    fn refresh_autocomplete(&mut self) {
        self.autocomplete
            .refresh(self.input.text(), self.input.cursor());
    }

    /// Cursor motion only keeps an ALREADY-OPEN session accurate (or closes
    /// it when the cursor leaves the token). Moving into an existing
    /// `@token` never surprise-opens the popup - pi behaves the same.
    fn refresh_autocomplete_if_open(&mut self) {
        if self.autocomplete.is_open() {
            self.refresh_autocomplete();
        }
    }

    /// Flip the mouse-capture state and tell the user what changed. Called
    /// by the event loop AFTER it issued the matching crossterm command;
    /// split from the key handler so the state logic is testable without a
    /// terminal. Returns the new state.
    pub fn apply_mouse_toggle(&mut self) -> bool {
        self.mouse_toggle_requested = false;
        self.mouse_captured = !self.mouse_captured;
        self.notice(if self.mouse_captured {
            "mouse capture on - wheel scrolls; ctrl+y to select/copy text"
        } else {
            "selection mode - select and copy with the mouse; ctrl+y to re-enable wheel scrolling"
        });
        self.mouse_captured
    }

    fn scroll_by(&mut self, delta: i64) {
        let max = self
            .last_total_lines
            .saturating_sub(self.last_transcript_height as usize);
        let next = i64::try_from(self.scroll_from_bottom).unwrap_or(0) + delta;
        self.scroll_from_bottom = usize::try_from(next.max(0)).unwrap_or(0).min(max);
    }

    /// Enter: dispatch commands locally, expand templates, or send
    /// the text as a prompt (steering when a run is active).
    fn submit(&mut self) {
        // Enter is consumed by the popup while open, so this is normally a
        // no-op - it exists so no code path can submit with a live session.
        self.autocomplete.close();
        let text = self.input.submit();
        let trimmed = text.trim().to_string();
        if trimmed.is_empty() {
            return;
        }
        if trimmed == "exit" || trimmed == "quit" {
            self.should_quit = true;
            return;
        }

        // /command dispatch, pi's order: built-ins are UI-local and never
        // reach the model; prompt templates expand into the prompt;
        // anything else falls through as literal text (a typo becomes a
        // question, not an error).
        if let Some(rest) = trimmed.strip_prefix('/') {
            if self.handle_builtin(rest) {
                self.scroll_from_bottom = 0;
                return;
            }
            let expanded = commands::expand_prompt_template(&trimmed, &self.meta.templates);
            if let Some(expanded) = expanded {
                self.send(&expanded);
                return;
            }
        }
        self.send(&trimmed);
    }

    /// Route a prompt to the agent: new run when idle, steering when busy.
    fn send(&mut self, text: &str) {
        // A prompt is headed for the agent - the "first interaction" moment
        // that scaffolds the project .cupel/ directory. Deliberately NOT at
        // startup (launching + quitting cupel must leave no trace), and not
        // for local built-ins like /help. Idempotent and never fails, so
        // calling it on every send is fine.
        crate::resources::ensure_project_dot_cupel(std::path::Path::new(&self.meta.cwd));
        if self.is_running() {
            // The agent injects steering messages after the current turn;
            // the transcript gets a real User cell when that happens (via
            // MessageEnd), so this marker only bridges the wait.
            self.agent.steer(AgentMessage::user_text(text));
            self.recorder.on_steer(text); // hook fires in the background
            self.transcript.cells.push(Cell::Queued {
                text: text.to_string(),
            });
        } else {
            // Not started here: the event loop takes it via
            // `take_pending_prompt`, awaits the prompt-path hooks
            // (session-start / user-prompt-submit, plus settling a pending
            // stop hook), THEN calls `start_run`. Keeping this method sync
            // keeps every key handler - and their tests - sync.
            self.pending_prompt = Some(text.to_string());
        }
        // New activity: snap back to following the output.
        self.scroll_from_bottom = 0;
    }

    /// Start the agent run for a prompt (the async half of `send`).
    pub fn start_run(&mut self, text: &str) {
        match self.agent.prompt_text(text) {
            Ok(events) => self.run_events = Some(events),
            Err(err) => self.transcript.cells.push(Cell::Error {
                text: err.to_string(),
            }),
        }
    }

    fn notice(&mut self, text: impl Into<String>) {
        self.transcript
            .cells
            .push(Cell::Notice { text: text.into() });
    }

    /// Handle a built-in `/command`. Returns false when the name isn't a
    /// built-in (the caller then tries prompt templates).
    fn handle_builtin(&mut self, rest: &str) -> bool {
        let (name, args) = rest
            .split_once(char::is_whitespace)
            .map_or((rest, ""), |(n, a)| (n, a.trim()));

        match name {
            "help" => {
                let mut lines = vec!["commands:".to_string()];
                for c in commands::BUILTIN_COMMANDS {
                    lines.push(format!("  /{}  - {}", c.name, c.description));
                }
                if !self.meta.templates.is_empty() {
                    lines.push("prompt templates:".to_string());
                    for t in &self.meta.templates {
                        lines.push(format!("  /{}  - {}", t.name, t.description));
                    }
                }
                self.notice(lines.join("\n"));
            }
            "usage" => {
                self.notice(format!(
                    "session totals: {} in / {} out / {} cached, ${:.4}",
                    self.totals.input, self.totals.output, self.totals.cache_read, self.totals.cost
                ));
            }
            "new" => {
                if self.is_running() {
                    self.notice("cannot clear while the agent is working (esc to abort first)");
                } else {
                    self.agent.reset();
                    self.transcript.cells.clear();
                    self.totals = Totals::default();
                    self.notice("conversation cleared");
                }
            }
            "model" => {
                if args.is_empty() {
                    let mut lines = vec!["available models (/model <id>):".to_string()];
                    for m in cupel_core::catalog::builtin_models() {
                        lines.push(format!("  {}  ({})", m.id, m.provider.as_str()));
                    }
                    self.notice(lines.join("\n"));
                } else if let Some(model) = cupel_core::catalog::builtin_models()
                    .into_iter()
                    .find(|m| m.id == args)
                {
                    self.meta.model_name = model.name.clone();
                    self.meta.provider = model.provider.as_str().to_string();
                    self.agent.set_model(model);
                    self.notice(format!(
                        "model switched to {args} (takes effect next request)"
                    ));
                } else {
                    self.notice(format!("unknown model: {args} (/model lists them)"));
                }
            }
            "thinking" => {
                let level = match args {
                    "off" => Some(None),
                    "minimal" => Some(Some(cupel_core::types::ThinkingLevel::Minimal)),
                    "low" => Some(Some(cupel_core::types::ThinkingLevel::Low)),
                    "medium" => Some(Some(cupel_core::types::ThinkingLevel::Medium)),
                    "high" => Some(Some(cupel_core::types::ThinkingLevel::High)),
                    "xhigh" => Some(Some(cupel_core::types::ThinkingLevel::XHigh)),
                    _ => None,
                };
                match level {
                    Some(level) => {
                        self.agent.set_thinking_level(level);
                        self.notice(format!("thinking level set to {args}"));
                    }
                    None => self
                        .notice("usage: /thinking off|minimal|low|medium|high|xhigh".to_string()),
                }
            }
            "quit" => self.should_quit = true,
            _ => return false,
        }
        true
    }

    // -----------------------------------------------------------------------
    // Agent events
    // -----------------------------------------------------------------------

    /// Await the next agent event, or park forever when no run is active.
    /// Parking (instead of returning None immediately) matters inside
    /// `tokio::select!`: a constantly-ready branch would busy-spin the loop.
    pub async fn next_agent_event(&mut self) -> Option<AgentEvent> {
        match &mut self.run_events {
            Some(events) => events.next().await,
            None => std::future::pending().await,
        }
    }

    pub async fn on_agent_event(&mut self, event: Option<AgentEvent>) {
        let Some(event) = event else {
            // Stream closed without AgentEnd (shouldn't happen; be robust).
            self.finish_run().await;
            return;
        };

        match event {
            AgentEvent::MessageUpdate { event } => match event {
                AssistantMessageEvent::TextDelta { delta, .. } => {
                    self.transcript.append_assistant(&delta);
                }
                AssistantMessageEvent::ThinkingDelta { delta, .. } => {
                    self.transcript.append_thinking(&delta);
                }
                AssistantMessageEvent::ToolCallEnd { tool_call, .. } => {
                    self.transcript.cells.push(Cell::Tool {
                        id: tool_call.id,
                        name: tool_call.name,
                        args: compact(&tool_call.arguments.to_string(), 200),
                        result: None,
                    });
                }
                _ => {}
            },

            // User messages (initial prompt and drained steering messages)
            // come back to us as events - the loop is the source of truth.
            AgentEvent::MessageEnd { message } => {
                // Every finalized message rides into the transcript file.
                self.recorder.record(&message);
                match message {
                    AgentMessage::Llm(Message::User(user)) => {
                        let text = match user.content {
                            UserContentBody::Text(text) => text,
                            UserContentBody::Blocks(_) => "(rich message)".to_string(),
                        };
                        self.transcript.cells.push(Cell::User { text });
                    }
                    AgentMessage::Llm(Message::Assistant(assistant)) => {
                        if matches!(
                            assistant.stop_reason,
                            StopReason::Error | StopReason::Aborted
                        ) {
                            self.transcript.cells.push(Cell::Error {
                                text: assistant
                                    .error_message
                                    .unwrap_or_else(|| "unknown error".to_string()),
                            });
                        } else {
                            let usage = &assistant.usage;
                            self.totals.input += usage.input;
                            self.totals.output += usage.output;
                            self.totals.cache_read += usage.cache_read;
                            self.totals.cost += usage.cost.total;
                            self.transcript.cells.push(Cell::Usage {
                                text: format!(
                                    "[{} in / {} out / {} cached, ${:.4}]",
                                    usage.input, usage.output, usage.cache_read, usage.cost.total
                                ),
                            });
                        }
                    }
                    _ => {}
                }
            }

            AgentEvent::ToolExecutionEnd {
                tool_call_id,
                result,
                is_error,
                ..
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
                self.transcript
                    .attach_tool_result(&tool_call_id, ToolOutcome { text, is_error });
            }

            AgentEvent::CompactionStart { reason } => {
                let cause = match reason {
                    cupel_agent::CompactionReason::Threshold => "context filling up",
                    cupel_agent::CompactionReason::Overflow => "context overflow",
                };
                self.transcript.cells.push(Cell::Notice {
                    text: format!("compacting context ({cause})..."),
                });
            }
            AgentEvent::CompactionEnd {
                tokens_before,
                tokens_after,
                error,
            } => {
                let text = match error {
                    None => format!(
                        "context compacted: ~{}k -> ~{}k tokens",
                        tokens_before / 1000,
                        tokens_after / 1000
                    ),
                    Some(error) => format!("compaction failed: {error}"),
                };
                self.transcript.cells.push(Cell::Notice { text });
            }

            AgentEvent::AutoRetry {
                attempt,
                max_attempts,
                delay_ms,
                error_message,
            } => {
                self.transcript.cells.push(Cell::Notice {
                    text: format!(
                        "retrying in {:.1}s (attempt {attempt}/{max_attempts}): {error_message}",
                        delay_ms as f64 / 1000.0
                    ),
                });
            }

            AgentEvent::AgentEnd { .. } => self.finish_run().await,
            _ => {}
        }

        // While following (offset 0) the view sticks to the newest output;
        // while scrolled up it stays put. Nothing to do either way - the
        // bottom-anchored render handles both.
    }

    async fn finish_run(&mut self) {
        self.run_events = None;
        // Fire the `stop` hook without blocking the UI; the next prompt's
        // before_prompt (or session end) settles it.
        self.recorder.on_agent_end();
        // Joins the (already finished) run tasks so state flags settle.
        self.agent.wait_for_idle().await;
    }
}

/// Truncate a one-line summary to at most `max_chars` characters.
fn compact(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let prefix: String = s.chars().take(max_chars).collect();
    format!("{prefix}...")
}
