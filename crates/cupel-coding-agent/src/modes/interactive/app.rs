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
    /// API keys entered via `/provider <name> <key>` this session. They
    /// take precedence over exported env vars and are NEVER persisted -
    /// process memory only (writing env vars back is impossible here:
    /// set_var is unsafe in edition 2024 and the workspace forbids unsafe).
    pub session_keys: std::collections::HashMap<String, String>,
    /// Set by `/hot-reload`; the async event loop performs the actual
    /// rebuild (it re-runs the bootstrap loader, which probes ollama and
    /// awaits hooks - nothing a sync key handler may do).
    pub pending_reload: Option<ReloadTarget>,
}

/// What `/hot-reload` asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReloadTarget {
    /// Bare `/hot-reload`: update the RUNNING session in place - same id,
    /// same history, same transcript file. Context-file changes arrive as
    /// an appended DELTA message, never as a re-embedded full file.
    Current,
    /// Resume the given session id: full rebuild with freshly loaded
    /// configuration (incl. a fresh system prompt), history seeded from
    /// that session's transcript.
    Resume(String),
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
        // meta.models is the MERGED catalog (builtins + models.json +
        // discovered ollama models), resolved once at startup.
        let model_candidates: Vec<Candidate> = meta
            .models
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
        let provider_candidates: Vec<Candidate> = crate::providers::catalog_providers(&meta.models)
            .into_iter()
            .map(|(provider, model)| Candidate {
                display: format!("{provider}  (default {})", model.id),
                value: provider,
                is_dir: false,
            })
            .collect();
        // `/hot-reload <id>` completes from the transcripts on disk. Only
        // file stems are read (no parsing) - App::new must stay fast.
        let session_candidates: Vec<Candidate> = recorder
            .sessions_dir()
            .map(list_session_id_candidates)
            .unwrap_or_default();
        let autocomplete = Autocomplete::new(&meta.cwd)
            .with_commands(command_candidates)
            .with_command_args("model", model_candidates)
            .with_command_args("thinking", thinking_candidates)
            .with_command_args("provider", provider_candidates)
            .with_command_args("hot-reload", session_candidates);
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
            session_keys: std::collections::HashMap::new(),
            pending_reload: None,
        };
        app.replay_history(&history);
        // A startup condition (e.g. keyless start) leads the transcript, so
        // it is the first thing the user reads - and scrolls away like any
        // other notice instead of blocking the session.
        if let Some(warning) = app.meta.startup_warning.take() {
            app.notice(warning);
        }
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

        // ---- autocomplete popup takes precedence while VISIBLE --------------
        // It consumes ONLY the keys it needs; everything else (Ctrl-C
        // included) falls through so session control never changes meaning.
        // Visible - not merely open: a session with zero matches renders
        // nothing, and an invisible popup swallowing Enter would make a
        // typo'd `/model xyz` un-submittable.
        if self.autocomplete.visible().is_some() {
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

    /// `/hot-reload`: apply `.cupel` changes, consuming the old App and
    /// returning its replacement (the event loop rebinds); on failure the
    /// OLD app comes back with an error notice, nothing torn down.
    ///
    /// What carries over in both modes: the current model + thinking level
    /// (runtime switches survive a reload), session-entered API keys, and
    /// the mouse-capture state.
    pub async fn hot_reload(self, target: ReloadTarget) -> Self {
        let cwd = std::path::PathBuf::from(&self.meta.cwd);
        match target {
            ReloadTarget::Current => self.reload_in_place(&cwd).await,
            ReloadTarget::Resume(id) => self.reload_resume(&cwd, &id).await,
        }
    }

    /// Bare `/hot-reload`: the RUNNING session continues - same id, same
    /// history, same transcript file, and no session-end hook (the session
    /// is not ending). Fresh templates, models, bash-deny rules, and tools
    /// are swapped in. Context files (AGENTS.md/CLAUDE.md) get DELTA
    /// treatment: the system prompt keeps the text embedded at session
    /// start, and only a unified diff of what changed on disk is appended
    /// to the conversation - the full file is never sent twice.
    async fn reload_in_place(self, cwd: &std::path::Path) -> Self {
        let state = self.agent.state();
        let registry = self.agent.registry();
        let ingredients = crate::bootstrap::load(cwd, self.meta.home.clone(), &registry).await;

        // The delta between what the session started with and what is on
        // disk now, as a user message the NEXT request will carry.
        let delta_message =
            crate::resources::context_delta(&self.meta.context_files, &ingredients.context_files)
                .map(AgentMessage::user_text);
        let mut seeded = state.messages.clone();
        if let Some(message) = &delta_message {
            seeded.push(message.clone());
        }

        let session_id = self.recorder.session_id().to_string();
        let mut options = cupel_agent::AgentOptions::new(state.model.clone(), registry);
        // Deliberately the OLD system prompt: the original context stays
        // embedded once; updates travel as the (small) delta message.
        options.system_prompt = state.system_prompt.clone();
        options.tools = ingredients.tools;
        options.hooks = std::sync::Arc::new(ingredients.guard);
        options.api_key = self.resolve_key(state.model.provider.as_str());
        options.thinking_level = state.thinking_level;
        options.tool_execution = cupel_agent::ToolExecutionMode::Parallel;
        options.session_id = Some(session_id.clone());
        options.messages = seeded;

        // Same id -> the new recorder APPENDS to the same transcript file.
        // The old recorder is dropped WITHOUT end_session: no session-end
        // hook fires, because this session is not ending.
        let recorder = crate::session::SessionRecorder::new(
            self.meta.home.clone(),
            cwd,
            &session_id,
            &state.model.id,
        );
        let meta = crate::modes::SessionMeta {
            model_name: state.model.name.clone(),
            provider: state.model.provider.as_str().to_string(),
            cwd: self.meta.cwd.clone(),
            templates: ingredients.templates,
            models: ingredients.models,
            home: self.meta.home.clone(),
            startup_warning: None,
            // The NEW files become the baseline, so the next reload diffs
            // against what this one already applied.
            context_files: ingredients.context_files,
        };

        let mut app = Self::new(cupel_agent::Agent::new(options), meta, recorder);
        app.session_keys = self.session_keys;
        app.mouse_captured = self.mouse_captured;
        if let Some(message) = delta_message {
            // The transcript file gets the update too - a later --resume
            // replays the same conversation the model saw.
            app.recorder.record(&message);
            app.notice(
                "configuration reloaded in place - context changes appended to the conversation",
            );
        } else {
            app.notice("configuration reloaded in place - no context file changes");
        }
        app
    }

    /// `/hot-reload <session-id>`: full rebuild with freshly loaded
    /// configuration (incl. a fresh system prompt - a resumed session gets
    /// the CURRENT context files embedded), history seeded from that
    /// session's transcript.
    async fn reload_resume(mut self, cwd: &std::path::Path, id: &str) -> Self {
        // Resolve the target session BEFORE tearing anything down.
        let Some(path) = self
            .recorder
            .sessions_dir()
            .map(|dir| dir.join(format!("{id}.jsonl")))
            .filter(|p| p.exists())
        else {
            self.notice(format!(
                "no session named {id} for this project (/session-id lists them)"
            ));
            return self;
        };
        let (session_id, seeded) = match crate::session::load_transcript(&path) {
            Ok((header, messages)) => (header.session_id, messages),
            Err(e) => {
                self.notice(format!("cannot resume {id}: {e}"));
                return self;
            }
        };

        // Close the old session cleanly: settles pending hooks and fires
        // session-end, so external consumers see a real boundary.
        self.recorder.end_session().await;

        let state = self.agent.state();
        let registry = self.agent.registry();
        let ingredients = crate::bootstrap::load(cwd, self.meta.home.clone(), &registry).await;

        let mut options = cupel_agent::AgentOptions::new(state.model.clone(), registry);
        options.system_prompt = ingredients.system_prompt;
        options.tools = ingredients.tools;
        options.hooks = std::sync::Arc::new(ingredients.guard);
        // The key is re-resolved for the CURRENT provider: session-entered
        // keys win, then env - same rule as /provider switching.
        options.api_key = self.resolve_key(state.model.provider.as_str());
        options.thinking_level = state.thinking_level;
        options.tool_execution = cupel_agent::ToolExecutionMode::Parallel;
        options.session_id = Some(session_id.clone());
        options.messages = seeded;

        let recorder = crate::session::SessionRecorder::new(
            self.meta.home.clone(),
            cwd,
            &session_id,
            &state.model.id,
        );
        let meta = crate::modes::SessionMeta {
            model_name: state.model.name.clone(),
            provider: state.model.provider.as_str().to_string(),
            cwd: self.meta.cwd.clone(),
            templates: ingredients.templates,
            models: ingredients.models,
            home: self.meta.home.clone(),
            // A reload is user-initiated; the startup condition was already
            // shown once and does not repeat.
            startup_warning: None,
            context_files: ingredients.context_files,
        };

        let mut app = Self::new(cupel_agent::Agent::new(options), meta, recorder);
        app.session_keys = self.session_keys;
        app.mouse_captured = self.mouse_captured;
        app.notice(format!(
            ".cupel configuration reloaded - resumed session {session_id}"
        ));
        app
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

    /// The API key for `provider`: a key entered this session wins, then
    /// the exported env var. Bedrock returns `None` - its AWS credential
    /// chain resolves inside the provider itself.
    fn resolve_key(&self, provider: &str) -> Option<String> {
        self.session_keys
            .get(provider)
            .cloned()
            .or_else(|| crate::providers::env_api_key(provider))
    }

    /// Point the agent at `model` AND re-resolve the API key for its
    /// provider. Model and key must travel together: switching providers
    /// while keeping the old key would sign requests with the wrong
    /// credential.
    fn switch_model(&mut self, model: cupel_core::types::Model) {
        let provider = model.provider.as_str().to_string();
        self.meta.model_name = model.name.clone();
        self.meta.provider = provider.clone();
        self.agent.set_api_key(self.resolve_key(&provider));
        self.agent.set_model(model);
    }

    /// `/provider` - list providers, or switch to one (optionally handing
    /// over an API key for this session).
    fn handle_provider_command(&mut self, args: &str) {
        let mut parts = args.split_whitespace();
        let name = parts.next().unwrap_or("");
        let entered_key = parts.next();

        if name.is_empty() {
            let mut lines = vec!["providers (/provider <name> [api-key]):".to_string()];
            for (provider, model) in crate::providers::catalog_providers(&self.meta.models) {
                let status = if provider == "amazon-bedrock" {
                    if crate::providers::has_aws_credentials() {
                        "AWS credentials found".to_string()
                    } else {
                        "no AWS credentials".to_string()
                    }
                } else if crate::providers::provider_is_keyless(&self.meta.models, &provider) {
                    // Local endpoints (ollama, llama-server): requests go
                    // out anonymously, nothing to configure.
                    "local endpoint - no key required".to_string()
                } else if self.session_keys.contains_key(&provider) {
                    "key entered this session".to_string()
                } else {
                    let var = crate::providers::env_var_name(&provider).unwrap_or("env");
                    if crate::providers::env_api_key(&provider).is_some() {
                        format!("{var} exported")
                    } else {
                        format!("no key ({var} unset)")
                    }
                };
                lines.push(format!("  {provider}  - default {}, {status}", model.id));
            }
            self.notice(lines.join("\n"));
            return;
        }

        let Some((provider, model)) = crate::providers::catalog_providers(&self.meta.models)
            .into_iter()
            .find(|(p, _)| p == name)
        else {
            self.notice(format!("unknown provider: {name} (/provider lists them)"));
            return;
        };

        if let Some(key) = entered_key {
            // Neither Bedrock (AWS chain) nor keyless local endpoints have
            // anywhere to put a key.
            if crate::providers::env_var_name(&provider).is_none()
                || crate::providers::provider_is_keyless(&self.meta.models, &provider)
            {
                self.notice(format!("{provider} does not take an API key - key ignored"));
            } else {
                // Session memory only, never persisted; deliberately not
                // echoed back into the transcript either.
                self.session_keys.insert(provider.clone(), key.to_string());
            }
        }

        // Describe where the credential comes from WITHOUT echoing it.
        let key_source = if provider == "amazon-bedrock" {
            "AWS credential chain".to_string()
        } else if crate::providers::provider_is_keyless(&self.meta.models, &provider) {
            "local endpoint - no key required".to_string()
        } else if self.session_keys.contains_key(&provider) {
            "using the key entered this session".to_string()
        } else if crate::providers::env_api_key(&provider).is_some() {
            format!(
                "using exported {}",
                crate::providers::env_var_name(&provider).unwrap_or("env")
            )
        } else {
            format!("NO KEY - requests will fail; set one with /provider {provider} <api-key>")
        };
        let model_id = model.id.clone();
        self.switch_model(model);
        self.notice(format!(
            "provider switched to {provider} (model {model_id}; {key_source})"
        ));
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
            "session-id" => {
                // Transcripts carry no stored summary (compaction output is
                // never persisted), so each session's FIRST PROMPT serves as
                // its human-readable label.
                let mut lines = vec![format!(
                    "current session: {} (resume later with `cupel --resume {}`)",
                    self.recorder.session_id(),
                    self.recorder.session_id()
                )];
                match self.recorder.sessions_dir() {
                    None => lines.push(
                        "session persistence is disabled (no cupel home) - nothing to list"
                            .to_string(),
                    ),
                    Some(dir) => {
                        let sessions = crate::session::list_sessions_in(dir);
                        lines.push(format!("sessions for this project ({}):", sessions.len()));
                        for s in &sessions {
                            let marker = if s.id == self.recorder.session_id() {
                                "*"
                            } else {
                                " "
                            };
                            lines.push(format!(
                                "{marker} {}  {}  {} msgs  {}  {}",
                                s.id,
                                crate::session::date_ymd(s.started_at),
                                s.message_count,
                                s.model,
                                s.label,
                            ));
                        }
                    }
                }
                self.notice(lines.join("\n"));
            }
            "review" => {
                // Builds the (truncated) code bundle synchronously - cheap
                // local fs/git work - then SENDS it like any prompt, so the
                // model call rides the normal async run path.
                let review_args = commands::parse_command_args(args);
                match crate::review::build_review_prompt(
                    std::path::Path::new(&self.meta.cwd),
                    &review_args,
                ) {
                    Ok(prompt) => self.send(&prompt),
                    Err(e) => self.notice(e),
                }
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
                    for m in &self.meta.models {
                        lines.push(format!("  {}  ({})", m.id, m.provider.as_str()));
                    }
                    self.notice(lines.join("\n"));
                } else if let Some(model) = self.meta.models.iter().find(|m| m.id == args).cloned()
                {
                    self.switch_model(model);
                    self.notice(format!(
                        "model switched to {args} (takes effect next request)"
                    ));
                } else {
                    self.notice(format!("unknown model: {args} (/model lists them)"));
                }
            }
            "provider" => self.handle_provider_command(args),
            "hot-reload" => {
                if self.is_running() {
                    self.notice(
                        "cannot hot-reload while the agent is working (esc to abort first)",
                    );
                } else {
                    // The event loop picks this up and rebuilds the session
                    // (async work); see App::hot_reload.
                    self.pending_reload = Some(if args.is_empty() {
                        ReloadTarget::Current
                    } else {
                        ReloadTarget::Resume(args.to_string())
                    });
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

/// Session-id completion candidates: transcript file stems, newest first
/// by modification time. Deliberately does NOT parse the transcripts -
/// this runs in `App::new`.
fn list_session_id_candidates(dir: &std::path::Path) -> Vec<Candidate> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut stems: Vec<(std::time::SystemTime, String)> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "jsonl"))
        .filter_map(|p| {
            let stem = p.file_stem()?.to_str()?.to_string();
            let modified = std::fs::metadata(&p).and_then(|m| m.modified()).ok()?;
            Some((modified, stem))
        })
        .collect();
    stems.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    stems
        .into_iter()
        .map(|(_, stem)| Candidate {
            display: stem.clone(),
            value: stem,
            is_dir: false,
        })
        .collect()
}

/// Truncate a one-line summary to at most `max_chars` characters.
fn compact(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let prefix: String = s.chars().take(max_chars).collect();
    format!("{prefix}...")
}
