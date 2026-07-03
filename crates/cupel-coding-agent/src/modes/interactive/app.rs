//! Application state and event handling.
//!
//! The TUI is a classic reducer: two event sources (terminal input, agent
//! events) mutate one `App`, and the render pass in `ui.rs` draws whatever
//! the `App` currently says. No state lives in the widgets - that's the
//! immediate-mode contract that keeps ratatui apps easy to reason about.

use futures_util::StreamExt as _;

use cupel_agent::{Agent, AgentEvent, AgentEventStream, AgentMessage};
use cupel_core::types::{
    AssistantMessageEvent, Message, StopReason, ToolResultContent, UserContentBody,
};
use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use super::input::InputState;
use super::transcript::{Cell, ToolOutcome, Transcript};
use crate::modes::SessionMeta;

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
}

impl App {
    #[must_use]
    pub fn new(agent: Agent, meta: SessionMeta) -> Self {
        Self {
            agent,
            meta,
            transcript: Transcript::default(),
            input: InputState::default(),
            totals: Totals::default(),
            run_events: None,
            scroll_from_bottom: 0,
            last_transcript_height: 0,
            last_total_lines: 0,
            should_quit: false,
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
            Event::Paste(text) => self.input.insert_str(&text),
            // Resize is handled implicitly: the next draw uses the new size.
            _ => {}
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);

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
            (KeyCode::Char('t'), true, _) => {
                self.transcript.expand_tools = !self.transcript.expand_tools;
            }
            (KeyCode::PageUp, ..) => {
                self.scroll_by(i64::from(self.last_transcript_height / 2).max(1));
            }
            (KeyCode::PageDown, ..) => {
                self.scroll_by(-i64::from(self.last_transcript_height / 2).max(-1));
            }

            // ---- editing --------------------------------------------------
            // Alt+Enter inserts a newline (Shift+Enter is indistinguishable
            // from Enter in most terminals, so Alt is the portable choice).
            (KeyCode::Enter, _, true) => self.input.insert('\n'),
            (KeyCode::Enter, ..) => self.submit(),
            (KeyCode::Backspace, ..) => self.input.delete_back(),
            (KeyCode::Delete, ..) => self.input.delete_forward(),
            (KeyCode::Left, ..) => self.input.move_left(),
            (KeyCode::Right, ..) => self.input.move_right(),
            (KeyCode::Home, ..) | (KeyCode::Char('a'), true, _) => self.input.move_home(),
            (KeyCode::End, ..) | (KeyCode::Char('e'), true, _) => self.input.move_end(),
            (KeyCode::Up, ..) => self.input.history_prev(),
            (KeyCode::Down, ..) => self.input.history_next(),
            (KeyCode::Char(c), false, false) => self.input.insert(c),
            _ => {}
        }
    }

    fn scroll_by(&mut self, delta: i64) {
        let max = self
            .last_total_lines
            .saturating_sub(self.last_transcript_height as usize);
        let next = i64::try_from(self.scroll_from_bottom).unwrap_or(0) + delta;
        self.scroll_from_bottom = usize::try_from(next.max(0)).unwrap_or(0).min(max);
    }

    /// Enter: start a run when idle, queue a steering message when busy.
    fn submit(&mut self) {
        let text = self.input.submit();
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return;
        }
        if trimmed == "exit" || trimmed == "quit" {
            self.should_quit = true;
            return;
        }

        if self.is_running() {
            // The agent injects steering messages after the current turn;
            // the transcript gets a real User cell when that happens (via
            // MessageEnd), so this marker only bridges the wait.
            self.agent.steer(AgentMessage::user_text(trimmed));
            self.transcript.cells.push(Cell::Queued {
                text: trimmed.to_string(),
            });
        } else {
            match self.agent.prompt_text(trimmed) {
                Ok(events) => self.run_events = Some(events),
                Err(err) => self.transcript.cells.push(Cell::Error {
                    text: err.to_string(),
                }),
            }
        }
        // New activity: snap back to following the output.
        self.scroll_from_bottom = 0;
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
            AgentEvent::MessageEnd { message } => match message {
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
            },

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
