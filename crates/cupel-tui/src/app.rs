//! Application state and event handling.
//!
//! The TUI is a classic reducer: two event sources (terminal input, agent
//! events) mutate one `App`, and the render pass in `ui.rs` draws whatever
//! the `App` currently says. No state lives in the widgets that's the
//! immediate-mode contract that keeps ratatui apps easy to reason about.

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::StreamExt as _;

use cupel_agent::{Agent, AgentEvent, AgentEventStream, AgentMessage};
use cupel_coding_agent::bootstrap::Frontend;
use cupel_coding_agent::commands;
use cupel_coding_agent::crew::{self, Changes, Crew, CrewError, CrewSetup, Reply};
use cupel_coding_agent::modes::SessionMeta;
use cupel_coding_agent::program_status::{BlockedKind, State as ProgramState};
use cupel_coding_agent::session::SessionRecorder;
use cupel_coding_agent::settings::Preset;
use cupel_coding_agent::tools::ask_user::{Answer, PendingQuestion, QuestionReceiver};
use cupel_coding_agent::usage::{HistoryUsage, UsageByModel, UsageTotals};
use cupel_core::types::{
    AssistantContent, AssistantMessageEvent, Message, StopReason, ToolResultContent,
    UserContentBody,
};
use ratatui::crossterm::event::{
    Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use ratatui::layout::{Position, Rect};
use tokio_util::sync::CancellationToken;

use crate::autocomplete::{Autocomplete, Candidate};
use crate::input::InputState;
use crate::login;
use crate::theme::Theme;
use crate::transcript::{Cell, ToolOutcome, Transcript};

use crate::usage::UsagePopup;

pub struct App {
    pub agent: Agent,
    pub meta: SessionMeta,
    pub transcript: Transcript,
    pub input: InputState,
    pub autocomplete: Autocomplete,
    pub totals: UsageTotals,
    pub usage_by_model: UsageByModel,
    pub usage_popup: Option<UsagePopup>,
    pub run_events: Option<AgentEventStream>,
    pub scroll_from_bottom: usize,
    pub last_transcript_height: u16,
    pub last_total_lines: usize,
    pub should_quit: bool,
    pub recorder: SessionRecorder,
    pub pending_prompt: Option<String>,
    pub queued: Vec<String>,
    pub steering: Vec<String>,
    pub mouse_captured: bool,
    pub mouse_toggle_requested: bool,
    pub session_keys: std::collections::HashMap<String, String>,
    pub pending_reload: Option<ReloadTarget>,
    pub login: Option<login::LoginFlow>,
    pub selected_cell: Option<usize>,
    pub tools_expanded: bool,
    pub frame: usize,
    pub context_tokens: u64,
    pub pending_copy: Option<String>,
    pub last_line_cells: Vec<Option<usize>>,
    pub last_transcript_inner: Rect,
    pub last_top_line: usize,
    pub last_answer: Status,
    pub pending_spinoff: Option<cupel_coding_agent::commands::SpinoffCommand>,
    pub update_requested: bool,
    pub pending_theme: Option<ThemeRequest>,
    pub themes: Vec<std::sync::Arc<Theme>>,
    pub run_finished: bool,
    pub activity: Activity,
    pub run_started: Option<Instant>,
    pub questions: Option<QuestionReceiver>,
    pub question: Option<OpenQuestion>,
    pub crew: Arc<Crew>,
    pub armed: Option<String>,
    pub helper: Option<HelperRun>,
    pub waiting_helper: Option<HelperCall>,
    pub routes: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HelperCall {
    pub name: String,
    pub task: String,
}

pub struct HelperRun {
    pub call: HelperCall,
    pub cell_id: String,
    pub events: tokio::sync::mpsc::UnboundedReceiver<HelperEvent>,
    pub cancel: CancellationToken,
}

impl Drop for HelperRun {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

pub enum HelperEvent {
    Progress(String),
    Done(Result<Reply, CrewError>),
}

const HELPER_SPEND_KIND: &str = "cupel.helper-spend";

/// A question from the `ask_user` tool, on screen until it is answered.
pub struct OpenQuestion {
    pub pending: PendingQuestion,
    pub selected: usize,
}

/// What a running agent does right now. The input box title shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Activity {
    Waiting,
    Thinking,
    Writing,
    CallingTool,
    Running(String),
    Retrying,
    Compacting,
}

/// `Display` gives every activity its label, so `format!("{activity}")` and
/// `.to_string()` both work.
impl core::fmt::Display for Activity {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Waiting => f.write_str("waiting for the model"),
            Self::Thinking => f.write_str("thinking"),
            Self::Writing => f.write_str("writing"),
            Self::CallingTool => f.write_str("writing a tool call"),
            Self::Running(tool) => write!(f, "running {tool}"),
            Self::Retrying => f.write_str("retrying"),
            Self::Compacting => f.write_str("compacting"),
        }
    }
}

/// How a prompt sent while the agent works reaches it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    Queue,
    Steer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    New,
    Running,
    Asking,
    Done,
    Failed,
    Cancelled,
}

/// What a new session takes over from the session that opens it. The model with its
/// thinking level and preset prompt, the API keys entered with `/provider`, and the
/// mouse state.
pub struct Carry {
    pub model: cupel_core::types::Model,
    pub thinking: Option<cupel_core::types::ThinkingLevel>,
    pub preset_prompt: String,
    pub session_keys: std::collections::HashMap<String, String>,
    pub mouse_captured: bool,
}

/// One wakeup from a background source (see [`App::next_event`]).
/// The size skew (AgentEvent is ~300 bytes, LoginEvent a Vec-sized
/// String) is fine for a value that lives one loop turn on the stack
/// boxing would buy nothing but an allocation per event.
#[allow(clippy::large_enum_variant)]
pub enum AppEvent {
    Agent(Option<AgentEvent>),
    Login(Option<login::LoginEvent>),
    Question(Option<PendingQuestion>),
    Usage(Result<HistoryUsage, tokio::task::JoinError>),
    Helper(Option<HelperEvent>),
}

/// What `/new` or `/hot-reload` asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReloadTarget {
    New,
    Current,
    Resume(String),
}

/// A `/themes` request handed to `Sessions` by the event loop. `Apply`
/// carries a resolved theme so an unknown id cannot reach the switch.
#[derive(Debug)]
pub enum ThemeRequest {
    List,
    Apply(std::sync::Arc<Theme>),
}

impl App {
    #[must_use]
    pub fn new(agent: Agent, meta: SessionMeta, recorder: SessionRecorder) -> Self {
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
        // after `/thinking ` the levels no more typing ids from memory.
        // meta.models is the merged catalog (builtins + models.json +
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
            ("high", "high thinking budget"),
            ("xhigh", "extra high thinking budget"),
            ("max", "maxium thinking budget"),
        ]
        .iter()
        .map(|(level, description)| Candidate {
            display: format!("{level}  - {description}"),
            value: (*level).to_string(),
            is_dir: false,
        })
        .collect();
        let provider_candidates: Vec<Candidate> =
            cupel_coding_agent::providers::catalog_providers(&meta.models)
                .into_iter()
                .map(|(provider, model)| Candidate {
                    display: format!("{provider}  (default {})", model.id),
                    value: provider,
                    is_dir: false,
                })
                .collect();
        let login_candidates = vec![Candidate {
            display:
                "openai-codex  - ChatGPT Plus/Pro browser login (append 'device' for headless)"
                    .to_string(),
            value: "openai-codex".to_string(),
            is_dir: false,
        }];
        // `/preset` offers the preset names from settings.json, sorted
        // (BTreeMap), each with what it switches to.
        let preset_candidates: Vec<Candidate> = meta
            .settings
            .presets
            .iter()
            .map(|(name, preset)| Candidate {
                display: format!("{name}  - {}", preset_summary(preset)),
                value: name.clone(),
                is_dir: false,
            })
            .collect();
        // /new, /hot-reload and every spinoff build a new App, so they
        // see new and edited theme files as well as the startup App.
        let (theme_files, theme_warnings) =
            cupel_coding_agent::themes::load_theme_files(meta.home.as_deref());
        let themes = crate::theme::registry(&theme_files);
        let theme_candidates: Vec<Candidate> = themes
            .iter()
            .map(|theme| Candidate {
                display: format!("{}  - {} ({})", theme.id, theme.name, theme.appearance),
                value: theme.id.to_string(),
                is_dir: false,
            })
            .collect();
        let logout_candidates = vec![Candidate {
            display: "openai-codex  - remove the stored ChatGPT login".to_string(),
            value: "openai-codex".to_string(),
            is_dir: false,
        }];
        let (specs, crew_warnings) =
            crew::resolve(&meta.settings, &meta.models, meta.home.as_deref());
        let crew = Arc::new(Crew::new(
            specs,
            CrewSetup {
                cwd: std::path::PathBuf::from(&meta.cwd),
                home: meta.home.clone(),
                registry: agent.registry(),
                settings: meta.settings.clone(),
                context_files: meta.context_files.clone(),
                session_id: recorder.session_id().into(),
            },
        ));
        let agent_candidates = crew
            .specs()
            .map(|spec| Candidate {
                display: format!(
                    "{}  - {}",
                    spec.name,
                    spec.definition.description().unwrap_or("manual only")
                ),
                value: spec.name.clone(),
                is_dir: false,
            })
            .collect();
        let (routes, route_warnings) = crew::routes(&meta.settings, &crew, &meta.templates);
        // `/hot-reload <id>` completes from the transcripts on disk. Only
        // file stems are read (no parsing) App::new must stay fast.
        let session_candidates: Vec<Candidate> = recorder
            .sessions_dir()
            .map(list_session_id_candidates)
            .unwrap_or_default();
        let autocomplete = Autocomplete::new(&meta.cwd)
            .with_commands(command_candidates)
            .with_command_args("model", model_candidates)
            .with_command_args("thinking", thinking_candidates)
            .with_command_args("themes", theme_candidates)
            .with_command_args("preset", preset_candidates)
            .with_command_args("provider", provider_candidates)
            .with_command_args("login", login_candidates)
            .with_command_args("logout", logout_candidates)
            .with_command_args("agent", agent_candidates)
            .with_command_args("hot-reload", session_candidates);
        // Restored history (a --resume session) exists before the App does;
        // snapshot it so the transcript can replay it as cells.
        let history = agent.state().messages;
        if let Some(spend) = history.iter().rev().find_map(|message| match message {
            AgentMessage::Custom { kind, payload, .. } if kind == HELPER_SPEND_KIND => {
                serde_json::from_value(payload.clone()).ok()
            }
            _ => None,
        }) {
            crew.restore_spend(spend);
        }
        let mut app = Self {
            agent,
            meta,
            transcript: Transcript::default(),
            input: InputState::default(),
            autocomplete,
            totals: UsageTotals::default(),
            usage_by_model: UsageByModel::default(),
            usage_popup: None,
            run_events: None,
            scroll_from_bottom: 0,
            last_transcript_height: 0,
            last_total_lines: 0,
            should_quit: false,
            recorder,
            pending_prompt: None,
            queued: Vec::new(),
            steering: Vec::new(),
            mouse_captured: true,
            mouse_toggle_requested: false,
            session_keys: std::collections::HashMap::new(),
            pending_reload: None,
            login: None,
            selected_cell: None,
            tools_expanded: false,
            frame: 0,
            context_tokens: 0,
            pending_copy: None,
            last_line_cells: Vec::new(),
            last_transcript_inner: Rect::ZERO,
            last_top_line: 0,
            last_answer: Status::New,
            pending_spinoff: None,
            update_requested: false,
            pending_theme: None,
            themes,
            run_finished: false,
            activity: Activity::Waiting,
            run_started: None,
            question: None,
            questions: None,
            crew,
            armed: None,
            helper: None,
            waiting_helper: None,
            routes,
        };
        app.replay_history(&history);
        for warning in std::mem::take(&mut app.meta.warnings)
            .into_iter()
            .chain(crew_warnings)
            .chain(route_warnings)
        {
            app.notice(warning);
        }
        for warning in theme_warnings {
            app.notice(warning);
        }
        // A startup condition (e.g. keyless start) also appears as a notice
        // instead of blocking the session.
        if let Some(warning) = app.meta.startup_warning.take() {
            app.notice(warning);
        }
        app.note_thinking_level();
        app
    }

    /// Say so when the model cannot do the thinking level that was asked for.
    /// Only "off" can differ (see `effective_thinking_level`); the footer
    /// shows the level that is really used.
    fn note_thinking_level(&mut self) {
        if self.agent.thinking_level().is_none()
            && let Some(level) = self.agent.effective_thinking_level()
        {
            let level = cupel_core::types::ModelThinkingLevel::from(level);
            self.notice(format!(
                "{} cannot turn thinking off, using {}",
                self.meta.model_name,
                level.as_str()
            ));
        }
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
                    let cell = match text.strip_prefix(cupel_agent::compaction::COMPACTION_MARKER) {
                        Some(summary) => Cell::Summary {
                            text: summary.trim_start().to_string(),
                        },
                        None => match crew::parse_exchange(&text) {
                            Some((name, body)) => Cell::Agent {
                                name: name.into(),
                                text: body.into(),
                            },
                            None => Cell::User { text },
                        },
                    };
                    self.transcript.cells.push(cell);
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
                            AssistantContent::ToolCall(tool_call) => Cell::Tool {
                                id: tool_call.id.clone(),
                                name: tool_call.name.clone(),
                                call: self
                                    .agent
                                    .describe_tool_call(&tool_call.name, &tool_call.arguments),
                                expanded: self.tools_expanded,
                                started_at: None,
                                live: None,
                                result: None,
                            },
                        };
                        self.transcript.cells.push(cell);
                    }
                    // Same error/usage bookkeeping as the live path, so
                    // /usage stays truthful across a resume.
                    self.account_assistant(assistant);
                }
                AgentMessage::Llm(Message::ToolResult(result)) => {
                    self.transcript.attach_tool_result(
                        &result.tool_call_id,
                        tool_outcome(&result.content, result.details.as_ref(), result.is_error),
                    );
                }
                // Custom messages are internal bookkeeping, not display.
                AgentMessage::Custom { .. } => {}
            }
        }
    }

    #[must_use]
    pub fn is_running(&self) -> bool {
        self.run_events.is_some() || self.helper.is_some()
    }

    /// Session's state for the sidebar.
    #[must_use]
    pub fn status(&self) -> Status {
        if self.question.is_some() {
            Status::Asking
        } else if self.is_running() {
            Status::Running
        } else {
            self.last_answer
        }
    }

    /// Protocol state is independent of which session is on screen. Stable
    /// activity labels avoid emitting a report for every token/reasoning headline.
    #[must_use]
    pub fn program_status(&self) -> (ProgramState, Option<String>) {
        if let Some(login) = &self.login {
            return match login.state() {
                login::LoginState::Working => (ProgramState::Working, Some("logging in".into())),
                login::LoginState::Waiting => (
                    ProgramState::Blocked(BlockedKind::Auth),
                    Some("complete login".into()),
                ),
            };
        }
        if self.last_answer == Status::Cancelled {
            return (ProgramState::Idle, None);
        }
        if let Some(question) = &self.question {
            return (
                ProgramState::Blocked(BlockedKind::Question),
                Some(question.pending.question().question.clone()),
            );
        }
        match self.status() {
            Status::Running => (ProgramState::Working, Some(self.activity.to_string())),
            Status::Done => (ProgramState::Done, None),
            Status::Failed => (ProgramState::Error, None),
            Status::New | Status::Asking | Status::Cancelled => (ProgramState::Idle, None),
        }
    }

    /// What a restart would lose in this session, or `None` when nothing would. A
    /// restart keeps what is on disk and nothing else.
    #[must_use]
    pub fn restart_blocker(&self) -> Option<&'static str> {
        if self.is_running() {
            return Some("is still working");
        }
        if !self.input.is_empty() {
            return Some("has unsent text in its input box");
        }
        if self.login.is_some() {
            return Some("waits for a login");
        }
        // `--resume` brings back what the transcript holds. When that is less than the
        // history (persistence is off, a write failed), the rest would be lost.
        let history = self.agent.state().messages.len();
        let saved = self
            .recorder
            .transcript()
            .and_then(|path| cupel_coding_agent::session::load_transcript(path).ok())
            .map_or(0, |(_, messages)| messages.len());
        (saved < history).then_some("has history its transcript file lacks")
    }

    pub fn tick(&mut self) {
        self.frame = self.frame.wrapping_add(1);
    }

    pub fn on_terminal_event(&mut self, event: Event) {
        if let Some(popup) = &mut self.usage_popup {
            // Preserve process control and the existing mouse-capture toggle.
            let global = matches!(
                &event,
                Event::Key(key) if key.kind == KeyEventKind::Press
                    && key.modifiers.contains(KeyModifiers::CONTROL)
                    && matches!(key.code, KeyCode::Char('c' | 'd' | 'y'))
            );
            if !global {
                if popup.on_terminal_event(&event) {
                    self.usage_popup = None;
                }
                return;
            }
        }
        match event {
            // Windows terminals report key releases too; only act on press.
            Event::Key(key) if key.kind == KeyEventKind::Press => self.on_key(key),
            Event::Paste(text) => {
                self.input.insert_str(&text);
                self.autocomplete
                    .refresh(self.input.text(), self.input.cursor());
            }
            // The wheel scrolls the transcript wherever the pointer is the
            // same clamped movement as PgUp/PgDn, at the conventional
            // 3-lines-per-notch step. A left click selects the block under
            // the pointer. (Mouse events only arrive because mod.rs
            // enables mouse capture.)
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::ScrollUp => self.scroll_by(3),
                MouseEventKind::ScrollDown => self.scroll_by(-3),
                MouseEventKind::Down(MouseButton::Left) => self.click(mouse.column, mouse.row),
                _ => {}
            },
            // Resize is handled implicitly: the next draw uses the new size.
            _ => {}
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);

        // It consumes only the keys it needs; everything else (Ctrl-C
        // included) falls through so session control never changes meaning.
        // Visible but  not merely open: a session with zero matches renders
        // nothing, and an invisible popup swallowing Enter would make a
        // typo'd `/model xyz` un-submittable.
        if self.autocomplete.visible().is_some() {
            match (key.code, ctrl, alt) {
                // Esc closes the popup it does not abort the run. A second
                // Esc (popup now closed) aborts as usual.
                (KeyCode::Esc, ..) => {
                    self.autocomplete.close();
                    return;
                }
                // Up/Down move the selection, not the prompt history.
                (KeyCode::Up, ..) => {
                    self.autocomplete.move_up();
                    return;
                }
                (KeyCode::Down, ..) => {
                    self.autocomplete.move_down();
                    return;
                }
                // Tab or Enter accept but Enter does not submit while open.
                (KeyCode::Tab, ..) | (KeyCode::Enter, false, false) => {
                    if let Some(completion) = self.autocomplete.accept(self.input.cursor()) {
                        self.input.replace_range(
                            completion.start,
                            completion.end,
                            &completion.insert,
                        );
                    }
                    // Refresh decides what happens next: a completed file no
                    // longer forms a token (trailing space) so the popup
                    // closes; a directory still does, so it keeps completing.
                    self.autocomplete
                        .refresh(self.input.text(), self.input.cursor());
                    return;
                }
                _ => {}
            }
        }

        // An open question takes the keys that answer it. Everything else works as
        // usual: typing writes an answer of one's own, Alt+Enter adds a line to it,
        // and Esc aborts the run.
        if let Some(open) = &mut self.question {
            let last = open.pending.question().options.len().saturating_sub(1);
            match (key.code, ctrl, alt) {
                (KeyCode::Up, _, false) => {
                    open.selected = open.selected.saturating_sub(1);
                    return;
                }
                (KeyCode::Down, _, false) => {
                    open.selected = (open.selected + 1).min(last);
                    return;
                }
                // Ctrl+Enter answers too: a steered prompt would wait behind the very
                // question it should answer.
                (KeyCode::Enter, _, false) | (KeyCode::Char('j'), true, _) => {
                    self.answer_question();
                    return;
                }
                _ => {}
            }
        }

        match (key.code, ctrl, alt) {
            (KeyCode::Char('c'), true, _) => {
                if self.is_running() {
                    // First Ctrl-C aborts the run; when idle it quits.
                    self.abort_run();
                } else {
                    self.should_quit = true;
                }
            }
            (KeyCode::Char('d'), true, _) if self.input.is_empty() && !self.is_running() => {
                self.should_quit = true;
            }
            // A waiting login is the newest interaction but the esc addresses
            // it first; a second esc then aborts the run as usual.
            (KeyCode::Esc, ..) if self.login.is_some() => {
                self.login = None; // Drop cancels the background task.
                self.notice("login cancelled");
            }
            (KeyCode::Esc, ..) if self.armed.is_some() => {
                if let Some(name) = self.armed.take() {
                    self.notice(format!("{name} no longer gets your next prompt"));
                }
            }
            (KeyCode::Esc, ..) if self.is_running() => self.abort_run(),
            // Esc while idle drops the click selection (harmless no-op
            // when nothing is selected).
            (KeyCode::Esc, ..) => self.selected_cell = None,

            // Ctrl+Y toggles "selection mode": mouse capture off so the
            // Terminal owns the mouse again (select + copy text natively),
            // then back on for wheel scrolling. Only requested here when the
            // event loop owns the terminal and issues the actual commands.
            (KeyCode::Char('y'), true, _) => self.mouse_toggle_requested = true,
            // Ctrl+O copies: the clicked block, or with nothing selected
            // - the latest answer, so the everyday "grab the result"
            // gesture needs no mouse at all.
            (KeyCode::Char('o'), true, _) => self.copy_selected(),
            // Ctrl+T shows every tool result in full (and hides them again).
            (KeyCode::Char('t'), true, _) => {
                self.tools_expanded = !self.tools_expanded;
                self.transcript.set_all_tools_expanded(self.tools_expanded);
            }
            (KeyCode::PageUp, ..) => {
                self.scroll_by(i64::from(self.last_transcript_height / 2).max(1));
            }
            (KeyCode::PageDown, ..) => {
                self.scroll_by(-i64::from(self.last_transcript_height / 2).max(1));
            }

            // Ctrl+Enter steers. A terminal reports it only with the keyboard
            // enhancement lib.rs switches on. Without it, most send a plain Enter.
            // Ctrl+J is the same key in every terminal. It sends the line feed
            // byte, which some terminals also send for Ctrl+Enter.
            (KeyCode::Enter | KeyCode::Char('j'), true, _) => self.submit(Delivery::Steer),
            // Alt+Enter inserts a newline (Shift+Enter is indistinguishable
            // from Enter in most terminals, so Alt is the portable choice).
            (KeyCode::Enter, _, true) => {
                self.input.insert('\n');
                self.refresh_autocomplete();
            }
            (KeyCode::Enter, ..) => self.submit(Delivery::Queue),
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
            (KeyCode::Up, _, true) => {
                if self.restore_queued() == 0 {
                    self.notice("no queued messages to restore");
                }
            }
            // History recall closes the popup instead of refreshing: a
            // recalled prompt containing `@src/x` must not surprise-open the
            // menu. Completion triggers on typing.
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

    /// Edits (typing, deleting, pasting) re-evaluate the popup and may open
    /// a session but  completion is typing-driven.
    fn refresh_autocomplete(&mut self) {
        self.autocomplete
            .refresh(self.input.text(), self.input.cursor());
    }

    /// Cursor motion only keeps an already-open session accurate (or closes
    /// it when the cursor leaves the token). Moving into an existing
    /// `@token` never surprise-opens the popup.
    fn refresh_autocomplete_if_open(&mut self) {
        if self.autocomplete.is_open() {
            self.refresh_autocomplete();
        }
    }

    /// `/new` or `/hot-reload`: rebuild from `.cupel` configuration, consuming
    /// the old App and returning its replacement (the event loop rebinds).
    /// A failed resume returns the old app with an error notice, nothing torn down.
    ///
    /// What carries over in all modes: the current model, thinking level,
    /// and preset prompt (runtime switches survive a reload), session-entered
    /// API keys (settings.json is re-read from disk), and the mouse-capture
    /// state.
    pub async fn hot_reload(self, target: ReloadTarget) -> Self {
        let cwd = std::path::PathBuf::from(&self.meta.cwd);
        match target {
            ReloadTarget::New => self.reload_new(&cwd).await,
            ReloadTarget::Current => self.reload_in_place(&cwd).await,
            ReloadTarget::Resume(id) => self.reload_resume(&cwd, &id).await,
        }
    }

    /// `/new`: end the old session and rebuild with an unused id and no history.
    async fn reload_new(self, cwd: &std::path::Path) -> Self {
        let mut timestamp = cupel_core::types::now_ms();
        let session_id = loop {
            let id = format!("cupel-{timestamp}");
            // Rapid resets or a resumed session must never reuse a transcript.
            if id != self.recorder.session_id()
                && !self
                    .recorder
                    .sessions_dir()
                    .is_some_and(|dir| dir.join(format!("{id}.jsonl")).exists())
            {
                break id;
            }
            timestamp += 1;
        };
        let mut app = self.reload_session(cwd, session_id, Vec::new()).await;
        app.notice(format!("new session {}", app.recorder.session_id()));
        app
    }

    /// Bare `/hot-reload`: the running session continues same id, same
    /// history, same transcript file, and no session-end hook (the session
    /// is not ending). Fresh templates, models, bash-deny rules, and tools
    /// are swapped in. Context files (AGENTS.md/CLAUDE.md) get delta
    /// treatment: the system prompt keeps the text embedded at session
    /// start, and only a unified diff of what changed on disk is appended
    /// to the conversation the full file is never sent twice.
    async fn reload_in_place(self, cwd: &std::path::Path) -> Self {
        let state = self.agent.state();
        let registry = self.agent.registry();
        let ingredients = cupel_coding_agent::bootstrap::load(
            cwd,
            self.meta.home.clone(),
            &registry,
            Frontend::Tui,
        )
        .await;
        let warning_suffix = if ingredients.warnings.is_empty() {
            ""
        } else {
            " with warnings"
        };

        // The delta between what the session started with and what is on
        // disk now, as a user message the next request will carry.
        let delta_message = cupel_coding_agent::resources::context_delta(
            &self.meta.context_files,
            &ingredients.context_files,
        )
        .map(AgentMessage::user_text);
        let mut seeded = state.messages.clone();
        if let Some(message) = &delta_message {
            seeded.push(message.clone());
        }

        let session_id = self.recorder.session_id().to_string();
        let mut options = cupel_agent::AgentOptions::new(state.model.clone(), registry);
        // Deliberately the old system prompt: the original context stays
        // embedded once; updates travel as the (small) delta message.
        options.system_prompt = state.system_prompt.clone();
        options.tools = ingredients.tools;
        options.hooks = std::sync::Arc::new(ingredients.hooks);
        let provider = state.model.provider.as_str();
        options.api_key = self.session_keys.get(provider).cloned().or_else(|| {
            cupel_coding_agent::providers::resolve_api_key(provider, &ingredients.settings)
        });
        options.thinking_level = state.thinking_level;
        options.tool_execution = cupel_agent::ToolExecutionMode::Parallel;
        options.session_id = Some(session_id);
        options.messages = seeded;

        let meta = cupel_coding_agent::modes::SessionMeta {
            model_name: state.model.name.clone(),
            provider: state.model.provider.as_str().to_string(),
            cwd: self.meta.cwd.clone(),
            templates: ingredients.templates,
            settings: ingredients.settings,
            models: ingredients.models,
            home: self.meta.home.clone(),
            startup_warning: None,
            warnings: ingredients.warnings,
            context_files: ingredients.context_files,
            base_system_prompt: self.meta.base_system_prompt.clone(),
        };

        // The session continues: retain its open transcript, started state,
        // and pending hook chain rather than creating a fresh recorder.
        let mut app = Self::new(cupel_agent::Agent::new(options), meta, self.recorder);
        app.questions = ingredients.questions;
        app.crew.remember_keys(&self.session_keys);
        app.session_keys = self.session_keys;
        app.mouse_captured = self.mouse_captured;
        if let Some(message) = delta_message {
            // The transcript file gets the update too a later --resume
            // replays the same conversation the model saw.
            app.recorder.record(&message);
            app.notice(format!(
                "configuration reloaded in place{warning_suffix} - context changes appended to the conversation"
            ));
        } else {
            app.notice(format!(
                "configuration reloaded in place{warning_suffix} - no context file changes"
            ));
        }
        app
    }

    /// `/hot-reload <session-id>`: full rebuild with freshly loaded
    /// configuration (incl. a fresh system prompt a resumed session gets
    /// the current context files embedded), history seeded from that
    /// session's transcript.
    async fn reload_resume(mut self, cwd: &std::path::Path, id: &str) -> Self {
        // Resolve the target session before tearing anything down.
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
        let (session_id, seeded) = match cupel_coding_agent::session::load_transcript(&path) {
            Ok((header, messages)) => (header.session_id, messages),
            Err(e) => {
                self.notice(format!("cannot resume {id}: {e}"));
                return self;
            }
        };

        let mut app = self.reload_session(cwd, session_id, seeded).await;
        app.notice(format!(
            ".cupel configuration reloaded - resumed session {}",
            app.recorder.session_id()
        ));
        app
    }

    /// Full session rebuild shared by `/new` and `/hot-reload <session-id>`.
    async fn reload_session(
        mut self,
        cwd: &std::path::Path,
        session_id: String,
        seeded: Vec<AgentMessage>,
    ) -> Self {
        // Close the old session cleanly: settles pending hooks and fires session-end,
        // so external consumers see a real boundary.
        self.recorder.end_session().await;
        let carry = self.carry();
        let registry = self.agent.registry();
        Self::open(
            cwd,
            self.meta.home.clone(),
            registry,
            carry,
            session_id,
            seeded,
        )
        .await
    }

    /// What the new session carries over from the original session ([`Carry`]).
    #[must_use]
    pub fn carry(&self) -> Carry {
        let state = self.agent.state();
        let preset_prompt = state
            .system_prompt
            .strip_prefix(self.meta.base_system_prompt.as_str())
            .unwrap_or_default()
            .to_string();
        Carry {
            model: state.model,
            thinking: state.thinking_level,
            preset_prompt,
            session_keys: self.session_keys.clone(),
            mouse_captured: self.mouse_captured,
        }
    }

    /// Build a session in `cwd` from freshly loaded configuration (the `.cupel`
    /// layers, context files and tools of that directory) plus what `carry` takes
    /// over. `/new`, `/hot-reload <session-id>` and `/spinoff` all open their session
    /// here.
    pub async fn open(
        cwd: &std::path::Path,
        home: Option<std::path::PathBuf>,
        registry: std::sync::Arc<cupel_core::provider::Registry>,
        carry: Carry,
        session_id: String,
        seeded: Vec<AgentMessage>,
    ) -> Self {
        let ingredients =
            cupel_coding_agent::bootstrap::load(cwd, home.clone(), &registry, Frontend::Tui).await;

        let mut options = cupel_agent::AgentOptions::new(carry.model.clone(), registry);
        options.system_prompt = format!("{}{}", ingredients.system_prompt, carry.preset_prompt);
        options.tools = ingredients.tools;
        options.hooks = std::sync::Arc::new(ingredients.hooks);
        // Session-entered keys still win, but the settings tier must come from the
        // fresh ingredients. A stale copy would lost hand edits.
        let provider = carry.model.provider.as_str();
        options.api_key = carry.session_keys.get(provider).cloned().or_else(|| {
            cupel_coding_agent::providers::resolve_api_key(provider, &ingredients.settings)
        });
        options.thinking_level = carry.thinking;
        options.tool_execution = cupel_agent::ToolExecutionMode::Parallel;
        options.session_id = Some(session_id.clone());
        options.messages = seeded;

        let recorder = cupel_coding_agent::session::SessionRecorder::new(
            home.clone(),
            cwd,
            &session_id,
            &carry.model.id,
        );
        let meta = cupel_coding_agent::modes::SessionMeta {
            model_name: carry.model.name.clone(),
            provider: provider.to_string(),
            cwd: cwd.display().to_string(),
            templates: ingredients.templates,
            models: ingredients.models,
            settings: ingredients.settings,
            home,
            // A startup condition was already shown once and does not repeat in
            // sessions opend later.
            startup_warning: None,
            warnings: ingredients.warnings,
            context_files: ingredients.context_files,
            base_system_prompt: ingredients.system_prompt,
        };

        let mut app = Self::new(cupel_agent::Agent::new(options), meta, recorder);
        app.questions = ingredients.questions;
        app.crew.remember_keys(&carry.session_keys);
        app.session_keys = carry.session_keys;
        app.mouse_captured = carry.mouse_captured;
        app
    }

    /// Flip the mouse-capture state and tell the user what changed. Called
    /// by the event loop after it issued the matching crossterm command;
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

    /// A left click selects the block under the pointer (the Ctrl+O copy
    /// target); clicking it again or clicking chrome deselects. A
    /// click on a tool cell toggles its preview instead. Terminal
    /// coordinates resolve through the geometry the render pass saved:
    /// window top line + row offset = visual line, and the line->cell map
    /// says which block that is.
    fn click(&mut self, column: u16, row: u16) {
        let inner = self.last_transcript_inner;
        if !inner.contains(Position { x: column, y: row }) {
            return;
        }
        let line = self.last_top_line + (row - inner.y) as usize;
        let hit = self.last_line_cells.get(line).copied().flatten();
        if let Some(index) = hit
            && matches!(self.transcript.cells.get(index), Some(Cell::Tool { .. }))
        {
            self.transcript.toggle_tool(index);
            return;
        }
        self.selected_cell = if hit == self.selected_cell { None } else { hit };
    }

    /// Ctrl+O: queue a block's raw text for the clipboard. The selected
    /// block wins; without a selection the most recent Answer is the
    /// target. Only queued here the event loop owns the terminal and
    /// emits the actual OSC 52 sequence (same split as the mouse toggle).
    fn copy_selected(&mut self) {
        let index = self.selected_cell.or_else(|| {
            self.transcript
                .cells
                .iter()
                .rposition(|cell| matches!(cell, Cell::Answer { .. } | Cell::Agent { .. }))
        });
        let Some(index) = index else {
            self.notice("nothing to copy - click a block, or finish a turn for an answer");
            return;
        };
        // Tool cells are never selectable (a click toggles them instead),
        // so copy_text only misses on stale state fail soft.
        let Some(text) = self.transcript.copy_text(index).map(str::to_string) else {
            return;
        };
        let chars = text.chars().count();
        self.pending_copy = Some(text);
        // "sent", not "copied": OSC 52 is fire-and-forget the terminal
        // decides whether it honors the sequence.
        self.notice(format!("sent {chars} chars to the clipboard (OSC 52)"));
    }

    fn scroll_by(&mut self, delta: i64) {
        let max = self
            .last_total_lines
            .saturating_sub(self.last_transcript_height as usize);
        let next = i64::try_from(self.scroll_from_bottom).unwrap_or(0) + delta;
        self.scroll_from_bottom = usize::try_from(next.max(0)).unwrap_or(0).min(max);
    }

    /// Enter or Ctrl+Enter: dispatch commands locally, expand templates, or
    /// send the text as a prompt (queued or steered when a run is active).
    fn submit(&mut self, delivery: Delivery) {
        // Enter is consumed by the popup while open, so this is normally a
        // no-op, so no code path can submit with a live session.
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

        // /command dispatch: built-ins are UI-local and never
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
                let name = rest.split_whitespace().next().unwrap_or_default();
                self.prompt_for(name, expanded, delivery);
                return;
            }
        }
        self.prompt_or_call(trimmed, delivery);
    }

    fn prompt_for(&mut self, command: &str, text: String, delivery: Delivery) {
        match self.routes.get(command).cloned() {
            Some(name) => self.call_helper(HelperCall { name, task: text }),
            None if command == "review"
                || self.meta.settings.agent_commands.contains_key(command) =>
            {
                self.send(&text, delivery);
            }
            None => self.prompt_or_call(text, delivery),
        }
    }

    fn prompt_or_call(&mut self, text: String, delivery: Delivery) {
        match self.armed.take() {
            Some(name) => self.call_helper(HelperCall { name, task: text }),
            None => self.send(&text, delivery),
        }
    }

    /// Enter on an open question. Typed text is the answer; with nothing typed, the
    /// highlighted option is.
    fn answer_question(&mut self) {
        let Some(open) = self.question.take() else {
            return;
        };
        self.autocomplete.close();
        let typed = self.input.submit();
        let answer = match typed.trim() {
            "" => Answer::Picked(open.selected),
            text => Answer::Typed(text.to_string()),
        };
        open.pending.answer(answer);
    }

    /// Esc or Ctrl-C while running. The queued and steered prompts go back
    /// into the input box first. An aborted run never reaches the point
    /// where it would take them, so they would sit in the agent's queues
    /// until the next run ends.
    pub(crate) fn abort_run(&mut self) {
        if let Some(helper) = &self.helper {
            helper.cancel.cancel();
        }
        let stopped = self.crew.stop_running();
        if !stopped.is_empty() && self.run_events.is_some() {
            self.notice(format!(
                "stopped {} - the lead goes on (esc again stops it too)",
                stopped.join(", ")
            ));
            return;
        }
        self.last_answer = Status::Cancelled;
        self.question = None;
        self.restore_queued();
        self.agent.abort();
    }

    /// Hand every waiting prompt back to the input box, steered ones first (they were
    /// due sooner), then queued ones, oldest first and ahead of whatever is typed.
    /// Returns how many came back.
    fn restore_queued(&mut self) -> usize {
        // Both copies go: a prompt that is back in the input box must not also reach
        // the model from the agent's queues.
        self.agent.clear_steering();
        self.agent.clear_follow_ups();
        // mem::take moves each list out and leaves an empty Vec behind.
        let waiting = self
            .waiting_helper
            .take()
            .map(|call| format!("/agent {} {}", call.name, call.task));
        let queued: Vec<String> = waiting
            .into_iter()
            .chain(core::mem::take(&mut self.steering))
            .chain(core::mem::take(&mut self.queued))
            .collect();
        let restored = queued.len();
        if restored > 0 {
            let typed = self.input.text().to_string();
            let typed_chars = typed.chars().count();
            let text = queued
                .into_iter()
                .chain([typed])
                .filter(|text| !text.trim().is_empty())
                .collect::<Vec<_>>()
                .join("\n\n");
            // Replacing the whole buffer (char range 0..len) sets the
            // text and puts the cursor at its end.
            self.input.replace_range(0, typed_chars, &text);
        }
        restored
    }

    /// Route a prompt to the agent: new run when idle, queued or steered
    /// when busy.
    pub fn send(&mut self, text: &str, delivery: Delivery) {
        // A prompt is headed for the agent the "first interaction" moment
        // that scaffolds the project .cupel/ directory. Deliberately not at
        // startup (launching + quitting cupel must leave no trace), and not
        // for local built-ins like /help. Idempotent and never fails, so
        // calling it on every send is fine.
        cupel_coding_agent::resources::ensure_project_dot_cupel(std::path::Path::new(
            &self.meta.cwd,
        ));
        if self.is_running() {
            let message = AgentMessage::user_text(text);
            match delivery {
                Delivery::Queue => {
                    self.agent.follow_up(message);
                    self.queued.push(text.to_string());
                }
                Delivery::Steer => {
                    self.agent.steer(message);
                    self.steering.push(text.to_string());
                }
            }
            self.recorder.on_queued_prompt(text);
        } else {
            // Not started here: the event loop takes it via
            // `take_pending_prompt`, awaits the prompt-path hooks
            // (session-start / user-prompt-submit, plus settling a pending
            // stop hook), then calls `start_run`. Keeping this method sync
            // keeps every key handler.
            self.pending_prompt = Some(text.to_string());
        }
        // New activity: snap back to following the output.
        self.scroll_from_bottom = 0;
    }

    /// Start the agent run for a prompt (the async half of `send`).
    pub fn start_run(&mut self, text: &str) {
        // The task leads the turn. `start_run`is the single funnel every
        // prompt goes through (typed prompts, expanded templates, /review bundles).
        self.transcript.cells.push(Cell::User {
            text: text.to_string(),
        });
        match self.agent.prompt_text(text) {
            Ok(events) => self.begin_run(events),
            Err(err) => {
                self.last_answer = Status::Failed;
                self.transcript.cells.push(Cell::Error {
                    text: err.to_string(),
                });
            }
        }
    }

    /// A run started: keep its events, start its clock, and wait for the model.
    fn begin_run(&mut self, events: AgentEventStream) {
        self.run_events = Some(events);
        self.run_started = Some(Instant::now());
        self.activity = Activity::Waiting;
        self.last_answer = Status::New;
    }

    /// The title's words: the headline of the reasoning while the model
    /// thinks (when it writes headlines), otherwise the activity.
    #[must_use]
    pub fn activity_label(&self) -> String {
        if let Some(helper) = &self.helper {
            return format!(
                "{} working · esc stops {}",
                helper.call.name, helper.call.name
            );
        }
        let helpers = self.crew.running_names();
        if !helpers.is_empty() {
            return format!("{} working · esc stops them", helpers.join(", "));
        }
        if self.question.is_some() {
            return "waiting for your answer".to_string();
        }
        if self.activity == Activity::Thinking
            && let Some(Cell::Thinking { text }) = self.transcript.cells.last()
            && let Some(headline) = reasoning_headline(text)
        {
            return headline.to_string();
        }
        self.activity.to_string()
    }

    fn account_assistant(&mut self, assistant: &cupel_core::types::AssistantMessage) {
        // Reported tokens are spent event if the response subsequently failed.
        self.usage_by_model.record(assistant);
        self.totals = self.usage_by_model.totals();
        if self.last_answer == Status::Cancelled {
            return;
        }
        // Steering cut this reply short on purpose. No error. The run goes on with
        // the steered prompt. Esc empties `steering` before it aborts, so an Esc never
        // lands here.
        if assistant.stop_reason == StopReason::Aborted && !self.steering.is_empty() {
            self.notice("interrupted to steer");
            return;
        }
        if matches!(
            assistant.stop_reason,
            StopReason::Error | StopReason::Aborted
        ) {
            self.last_answer = Status::Failed;
            self.transcript.cells.push(Cell::Error {
                text: assistant
                    .error_message
                    .clone()
                    .unwrap_or_else(|| "unkown error".to_string()),
            });
            return;
        }
        self.last_answer = Status::Done;
        if assistant.stop_reason == StopReason::Length {
            self.transcript.cells.push(Cell::Error {
                text: "response was truncated before completion (output token limit)".to_string(),
            });
        }
        let usage = &assistant.usage;
        self.context_tokens = if usage.total_tokens > 0 {
            usage.total_tokens
        } else {
            usage.input + usage.output + usage.cache_read + usage.cache_write
        };
        // Reasoning tokens are part of `out`. Only some providers report them
        // separately, so the note appears only when there is a count.
        let reasoning = match usage.reasoning {
            Some(tokens) if tokens > 0 => format!(" ({tokens} reasoning)"),
            _ => String::new(),
        };
        self.transcript.cells.push(Cell::Usage {
            text: format!(
                "[{} in / {} out{reasoning} / {} cached, ${:.4}]",
                usage.input, usage.output, usage.cache_read, usage.cost.total
            ),
        });
    }

    pub fn notice(&mut self, text: impl Into<String>) {
        self.transcript
            .cells
            .push(Cell::Notice { text: text.into() });
    }

    /// The API key for `provider`: a key entered this session wins, then
    /// the exported env var. Bedrock returns `None`. Its AWS credential
    /// chain resolves inside the provider itself.
    fn resolve_key(&self, provider: &str) -> Option<String> {
        self.session_keys.get(provider).cloned().or_else(|| {
            cupel_coding_agent::providers::resolve_api_key(provider, &self.meta.settings)
        })
    }

    /// Point the agent at `model` and re-resolve the API key for its
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

    /// `/provider` list providers, or switch to one (optionally handing
    /// over an API key, which is kept for the session and saved to
    /// ~/.cupel/settings.json).
    fn handle_provider_command(&mut self, args: &str) {
        let mut parts = args.split_whitespace();
        let name = parts.next().unwrap_or("");
        let entered_key = parts.next();

        if name.is_empty() {
            let mut lines = vec!["providers (/provider <name> [api-key]):".to_string()];
            for (provider, model) in
                cupel_coding_agent::providers::catalog_providers(&self.meta.models)
            {
                // The order of these arms mirrors resolve_key's precedence
                // (session > env > settings) keep the two in sync, or the
                // listing lies about which key a request would use.
                let status = if provider == "amazon-bedrock" {
                    if cupel_coding_agent::providers::has_aws_credentials() {
                        "AWS credentials found".to_string()
                    } else {
                        "no AWS credentials".to_string()
                    }
                } else if provider == "openai-codex" {
                    // Subscription auth: the credential is a stored login,
                    // never a key mirror auth.json, not the key tiers.
                    if cupel_coding_agent::auth::has_credential(
                        self.meta.home.as_deref(),
                        &provider,
                    ) {
                        // A stored login, not proof that its refresh still works.
                        "ChatGPT login stored (/logout openai-codex)".to_string()
                    } else {
                        "not logged in - /login openai-codex".to_string()
                    }
                } else if cupel_coding_agent::providers::provider_is_keyless(
                    &self.meta.models,
                    &provider,
                ) {
                    // Local endpoints (ollama, llama-server): requests go
                    // out anonymously, nothing to configure.
                    "local endpoint - no key required".to_string()
                } else if self.session_keys.contains_key(&provider) {
                    if self.meta.settings.api_key(&provider).is_some() {
                        "key entered this session & saved to settings.json".to_string()
                    } else {
                        "key entered this session".to_string()
                    }
                } else if let Some((var, _)) =
                    cupel_coding_agent::providers::exported_key(&provider)
                {
                    if self.meta.settings.api_key(&provider).is_some() {
                        format!("{var} exported (overrides settings.json)")
                    } else {
                        format!("{var} exported")
                    }
                } else if self.meta.settings.api_key(&provider).is_some() {
                    "key in settings.json".to_string()
                } else if let Some(var) = cupel_coding_agent::providers::env_var_name(&provider) {
                    format!("no key ({var} unset)")
                } else {
                    // Custom providers have no env var at all pointing at
                    // one would be misleading; /provider is ther channel.
                    format!("no key (set with /provider {provider} <api-key>")
                };
                lines.push(format!("  {provider}  - default {}, {status}", model.id));
            }
            self.notice(lines.join("\n"));
            return;
        }

        let Some((provider, model)) =
            cupel_coding_agent::providers::catalog_providers(&self.meta.models)
                .into_iter()
                .find(|(p, _)| p == name)
        else {
            self.notice(format!("unknown provider: {name} (/provider lists them)"));
            return;
        };

        let mut saved_note = String::new();
        if let Some(key) = entered_key {
            // Neither Bedrock (AWS chain) nor keyless local endpoints have
            // anywhere to put a key.
            if !cupel_coding_agent::providers::takes_api_key(&self.meta.models, &provider) {
                self.notice(format!("{provider} does not take an API key - key ignored"));
            } else {
                // Session memory first: the key must work for this session
                // even if the disk save below fails. Deliberately never
                // echoed back into the transcript.
                self.session_keys.insert(provider.clone(), key.to_string());
                self.crew.remember_key(&provider, key);
                // Persist. save_provider_key re-reads the file itself and
                // refuses to clobber a malformed one; the in-memory mirror
                // is updated only on sucess
                let saved = cupel_coding_agent::settings::save_provider_key(
                    self.meta.home.as_deref(),
                    &provider,
                    key,
                );
                match saved {
                    Ok(path) => {
                        // Mirror the write so the /provider listing (which
                        // reads meta.settings, never the disk) stays true.
                        self.meta
                            .settings
                            .providers
                            .insert(provider.clone(), key.to_string());
                        saved_note = format!("; key saved to {}", path.display());
                    }
                    // SaveError carries paths and parse reasons, never the
                    // key safe to interpolate.
                    Err(e) => self.notice(format!(
                        "warning: key not saved ({e}) - it stays active for this session only"
                    )),
                }
            }
        }

        // Describe where the credential comes from without echoing it.
        let key_source = if provider == "amazon-bedrock" {
            "AWS credential chain".to_string()
        } else if provider == "openai-codex" {
            if cupel_coding_agent::auth::has_credential(self.meta.home.as_deref(), &provider) {
                "using the stored ChatGPT login (tokens auto-refresh)".to_string()
            } else {
                "NOT logged in - requests will fail; run /login openai-codex".to_string()
            }
        } else if cupel_coding_agent::providers::provider_is_keyless(&self.meta.models, &provider) {
            "local endpoint - no key required".to_string()
        } else if self.session_keys.contains_key(&provider) {
            "using the key entered this session".to_string()
        } else if let Some((var, _)) = cupel_coding_agent::providers::exported_key(&provider) {
            format!("using exported {var}")
        } else if self.meta.settings.api_key(&provider).is_some() {
            "using the key from settings.json".to_string()
        } else {
            format!("NO KEY - requests will fail; set one with /provider {provider} <api-key>")
        };
        let model_id = model.id.clone();
        self.switch_model(model);
        self.notice(format!(
            "provider switched to {provider} (model {model_id}; {key_source} {saved_note})"
        ));
        self.note_thinking_level();
    }

    /// `/preset`, list the presets from settings.json, or switched to one:
    /// model (with its key), thinking level, and system prompt together, each
    /// taking effect with the next request like /model and /thinking.
    fn handle_preset_command(&mut self, name: &str) {
        if name.is_empty() {
            if self.meta.settings.presets.is_empty() {
                self.notice(
                    "no presets defined - add a \"model\" section to ~/.cupel/settings.json",
                );
                return;
            }
            let mut lines = vec!["presets (/presets <name>):".to_string()];
            for (name, preset) in &self.meta.settings.presets {
                lines.push(format!("  {name}  - {}", preset_summary(preset)));
            }
            self.notice(lines.join("\n"));
            return;
        }
        // Cloned out of self.meta: switch_model below needs `&mut self`,
        // which a live reference into self.meta.settings would forbid.
        let Some(preset) = self.meta.settings.presets.get(name).cloned() else {
            self.notice(format!("unknown preset: {name} (/preset lists them"));
            return;
        };
        let Some(model) = preset.find_model(&self.meta.models).cloned() else {
            self.notice(format!(
                "preset {name}: unknown model {:?}/{} (/model lists them)",
                preset.provider, preset.model
            ));
            return;
        };
        self.switch_model(model);
        self.agent.set_thinking_level(preset.thinking_level.level());
        self.agent
            .set_system_prompt(cupel_coding_agent::system_prompt::with_preset_prompt(
                &self.meta.base_system_prompt,
                preset.prompt.as_deref(),
            ));
        self.notice(format!(
            "preset {name} activated: {} (takes effect next request)",
            preset_summary(&preset)
        ));
        self.note_thinking_level();
    }

    #[must_use]
    pub fn session_totals(&self) -> UsageTotals {
        let helpers = self.crew.total_spend();
        let mut totals = self.totals;
        totals.input += helpers.input;
        totals.output += helpers.output;
        totals.cache_read += helpers.cache_read;
        totals.cost += helpers.cost;
        totals
    }

    fn show_helper_usage(&mut self) {
        if self.crew.total_spend().calls == 0 {
            return;
        }
        let totals = self.session_totals();
        let mut lines = vec![format!(
            "session including helpers: {} in / {} out / {} cached, ${:.4}",
            totals.input, totals.output, totals.cache_read, totals.cost
        )];
        lines.push(format!(
            "  lead: {} in / {} out, ${:.4}",
            self.totals.input, self.totals.output, self.totals.cost
        ));
        for (name, spend) in self.crew.spend_snapshot() {
            lines.push(format!(
                "  {name}: {} calls, {} in / {} out / {} cached, ${:.4}",
                spend.calls, spend.input, spend.output, spend.cache_read, spend.cost
            ));
        }
        self.notice(lines.join("\n"));
    }

    fn handle_agent_command(&mut self, args: &str) {
        let (name, task) = args
            .split_once(char::is_whitespace)
            .map_or((args, ""), |(name, task)| (name, task.trim()));
        if name.is_empty() {
            self.list_team();
            return;
        }
        if self.crew.spec(name).is_none() {
            self.notice(format!("no agent named {name} (/team lists them)"));
            return;
        }
        if task.is_empty() {
            self.armed = Some(name.into());
            self.notice(format!("your next prompt goes to {name} (esc cancels)"));
        } else {
            self.call_helper(HelperCall {
                name: name.into(),
                task: task.into(),
            });
        }
    }

    fn list_team(&mut self) {
        let mut lines = vec!["agents (/agent <name> [task]):".to_string()];
        for spec in self.crew.specs() {
            let tools = spec
                .definition
                .tools()
                .iter()
                .map(|tool| tool.as_str())
                .collect::<Vec<_>>()
                .join(" ");
            lines.push(format!(
                "  {} - {}/{}, thinking {}, tools {tools}, {}, from {}",
                spec.name,
                spec.model.provider.as_str(),
                spec.model.id,
                spec.definition.preset.thinking_level.as_str(),
                if spec.definition.writes() {
                    "can write"
                } else {
                    "read-only"
                },
                spec.definition.origin.as_str()
            ));
            lines.push(format!(
                "      {}",
                spec.definition
                    .description()
                    .unwrap_or("manual only: no description")
            ));
            let commands = self
                .routes
                .iter()
                .filter(|(_, agent)| **agent == spec.name)
                .map(|(command, _)| format!("/{command}"))
                .collect::<Vec<_>>();
            if !commands.is_empty() {
                lines.push(format!("      answers {}", commands.join(", ")));
            }
            if let Some(max) = spec.definition.max_turns {
                lines.push(format!("      max {max} turns"));
            }
            if !spec.definition.remembers() {
                lines.push("      forgets between calls".into());
            }
            let spend = self.crew.spend_of(&spec.name);
            lines.push(if spend.calls > 0 {
                format!(
                    "      {} calls, {} in / {} out, ${:.4}",
                    spend.calls, spend.input, spend.output, spend.cost
                )
            } else {
                "      not called yet".into()
            });
        }
        if self.crew.specs().next().is_none() {
            self.notice("no agents defined - add an \"agents\" section to ~/.cupel/settings.json or .cupel/settings.json");
        } else {
            self.notice(lines.join("\n"));
        }
    }

    fn call_helper(&mut self, call: HelperCall) {
        cupel_coding_agent::resources::ensure_project_dot_cupel(std::path::Path::new(
            &self.meta.cwd,
        ));
        if !self.is_running() {
            self.start_helper(call);
        } else if self.waiting_helper.is_none() {
            self.notice(format!("{} starts when the current run is done", call.name));
            self.waiting_helper = Some(call);
        } else {
            self.notice(
                "another agent call is waiting already - send this one when it has started",
            );
        }
        self.scroll_from_bottom = 0;
    }

    fn start_helper(&mut self, call: HelperCall) {
        self.transcript.cells.push(Cell::User {
            text: format!("@{} {}", call.name, call.task),
        });
        let cell_id = format!("helper-{}", self.transcript.cells.len());
        self.transcript.cells.push(Cell::Tool {
            id: cell_id.clone(),
            name: call.name.clone(),
            call: format!(
                "{}: {}",
                call.name,
                call.task.lines().next().unwrap_or_default()
            ),
            expanded: self.tools_expanded,
            started_at: Some(Instant::now()),
            live: None,
            result: None,
        });
        let (sender, events) = tokio::sync::mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let run_cancel = cancel.clone();
        let crew = Arc::clone(&self.crew);
        let (name, task) = (call.name.clone(), call.task.clone());
        tokio::spawn(async move {
            let progress_sender = sender.clone();
            let progress = move |lines| {
                let _ = progress_sender.send(HelperEvent::Progress(lines));
            };
            let result = crew.run(&name, &task, run_cancel, &progress).await;
            let _ = sender.send(HelperEvent::Done(result));
        });
        self.run_started = Some(Instant::now());
        self.last_answer = Status::New;
        self.helper = Some(HelperRun {
            call,
            cell_id,
            events,
            cancel,
        });
    }

    pub fn on_helper_event(&mut self, event: Option<HelperEvent>) {
        match event {
            Some(HelperEvent::Progress(lines)) => {
                if let Some(run) = &self.helper {
                    self.transcript.attach_tool_progress(&run.cell_id, lines);
                }
            }
            Some(HelperEvent::Done(result)) => {
                if let Some(run) = self.helper.take() {
                    self.finish_helper(&run, result);
                }
            }
            None => {
                if let Some(run) = self.helper.take() {
                    run.cancel.cancel();
                    let error = CrewError::Failed { name: run.call.name.clone(), message: "helper task ended without reporting; inspect the working tree before continuing".into(), changes: Changes::Unknown };
                    self.finish_helper(&run, Err(error));
                }
            }
        }
    }

    fn finish_helper(&mut self, run: &HelperRun, result: Result<Reply, CrewError>) {
        self.run_started = None;
        match result {
            Ok(reply) => {
                self.transcript.attach_tool_result(
                    &run.cell_id,
                    ToolOutcome {
                        text: format!("{} tool calls{}", reply.tool_calls, reply.changes),
                        is_error: false,
                        diff: None,
                        took: None,
                    },
                );
                self.transcript.cells.push(Cell::Agent {
                    name: reply.name.clone(),
                    text: reply.answer.clone(),
                });
                self.transcript.cells.push(Cell::Usage {
                    text: format!(
                        "[{} in / {} out / {} cached, ${:.4}]",
                        reply.spend.input,
                        reply.spend.output,
                        reply.spend.cache_read,
                        reply.spend.cost
                    ),
                });
                let note = AgentMessage::user_text(crew::exchange_note(&run.call.task, &reply));
                self.recorder.record(&note);
                self.agent.push_message(note);
                self.last_answer = Status::Done;
            }
            Err(error) => {
                self.transcript.attach_tool_result(
                    &run.cell_id,
                    ToolOutcome {
                        text: error.to_string(),
                        is_error: true,
                        diff: None,
                        took: None,
                    },
                );
                let note = AgentMessage::user_text(format!(
                    "{} {}\nThe helper call did not finish successfully:\n{}",
                    crew::EXCHANGE_MARKER,
                    run.call.name,
                    error
                ));
                self.recorder.record(&note);
                self.agent.push_message(note);
                if matches!(error, CrewError::Stopped { .. }) {
                    self.last_answer = Status::Cancelled;
                    self.notice(error.to_string());
                } else {
                    self.last_answer = Status::Failed;
                    self.transcript.cells.push(Cell::Error {
                        text: error.to_string(),
                    });
                }
            }
        }
        self.record_helper_spend();
        self.run_finished = true;
        self.start_waiting();
    }

    fn record_helper_spend(&mut self) {
        let spend = self.crew.spend_snapshot();
        if spend.is_empty() {
            return;
        }
        let message = AgentMessage::Custom {
            kind: HELPER_SPEND_KIND.into(),
            payload: serde_json::json!(spend),
            timestamp: cupel_core::types::now_ms(),
        };
        self.recorder.record(&message);
        self.agent.push_message(message);
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
            "session-id" if !args.is_empty() => {
                self.notice(format!(
                    "invalid argument for /session-id: {args:?}; this command takes no arguments.\n\
                     Use /session-id to list valid session IDs, or /hot-reload <session-id> to resume one."
                ));
            }
            "session-id" => {
                // Transcripts carry no stored summary (compaction output is
                // never persisted), so each session's first prompt serves as
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
                        let sessions = cupel_coding_agent::session::list_sessions_in(dir);
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
                                cupel_coding_agent::session::date_ymd(s.started_at),
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
                // Builds the (truncated) code bundle synchronously cheap
                // local fs/git work then sends it like any prompt, so the
                // model call rides the normal async run path.
                let review_args = commands::parse_command_args(args);
                match cupel_coding_agent::review::build_review_prompt(
                    std::path::Path::new(&self.meta.cwd),
                    &review_args,
                ) {
                    Ok(prompt) => self.prompt_for("review", prompt, Delivery::Queue),
                    Err(e) => self.notice(e),
                }
            }
            "usage" => {
                self.show_helper_usage();
                // Derive the identiy even if a recorder write failure cleared its path.
                let exclude = cupel_coding_agent::session::sessions_dir(
                    self.meta.home.as_deref(),
                    std::path::Path::new(&self.meta.cwd),
                )
                .map(|dir| dir.join(format!("{}.jsonl", self.recorder.session_id())));
                self.usage_popup = Some(UsagePopup::new(
                    self.usage_by_model.clone(),
                    self.meta.home.clone(),
                    exclude,
                ));
            }
            "new" => {
                if self.is_running() {
                    self.notice("cannot clear while the agent is working (esc to abort first)");
                } else {
                    // Ending hooks and rebuilding the recorder require async
                    // work; the event loop handles this like /hot-reload.
                    self.pending_reload = Some(ReloadTarget::New);
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
                    self.note_thinking_level();
                } else {
                    self.notice(format!("unknown model: {args} (/model lists them)"));
                }
            }
            "provider" => self.handle_provider_command(args),
            "agent" => self.handle_agent_command(args),
            "team" => self.list_team(),
            "preset" => self.handle_preset_command(args),
            "login" => self.handle_login_command(args),
            "logout" => self.handle_logout_command(args),
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
                    "max" => Some(Some(cupel_core::types::ThinkingLevel::Max)),
                    _ => None,
                };
                match level {
                    Some(level) => {
                        self.agent.set_thinking_level(level);
                        self.notice(format!("thinking level set to {args}"));
                        self.note_thinking_level();
                    }
                    None => self.notice(
                        "usage: /thinking off|minimal|low|medium|high|xhigh|max".to_string(),
                    ),
                }
            }
            "spinoff" => match args.parse() {
                Ok(command) => self.pending_spinoff = Some(command),
                Err(error) => self.notice(format!("{error}")),
            },
            "themes" if args.is_empty() => self.pending_theme = Some(ThemeRequest::List),
            "themes" => match crate::theme::find(&self.themes, args) {
                Some(theme) => self.pending_theme = Some(ThemeRequest::Apply(theme)),
                None => self.notice(format!("unknown theme: {args} (/themes lists them)")),
            },
            "quit" => self.should_quit = true,
            "update" => self.update_requested = true,
            _ => return false,
        }
        true
    }

    /// `/login` start a subscription login, or feed a pasted redirect
    /// into the one that is waiting.
    fn handle_login_command(&mut self, args: &str) {
        let mut parts = args.split_whitespace();
        let provider = parts.next().unwrap_or("");
        let argument = parts.next();

        if provider.is_empty() {
            self.notice(
                "subscription logins:\n  \
                 /login openai-codex          - ChatGPT Plus/Pro via browser\n  \
                 /login openai-codex device   - device-code login (headless)\n\
                 while a browser login waits, /login openai-codex <redirect-url> \
                 completes it manually",
            );
            return;
        }
        if provider != "openai-codex" {
            self.notice(format!(
                "unknown login provider: {provider} (/login lists them)"
            ));
            return;
        }

        match argument {
            None => self.start_login(login::spawn_browser(self.meta.home.clone()), "browser"),
            Some("device") => {
                self.start_login(login::spawn_device(self.meta.home.clone()), "device-code");
            }
            // Anything else is the manual fallback: the redirect URL (or
            // bare code) pasted into the flow that is waiting for it.
            Some(pasted) => match &mut self.login {
                Some(flow) => {
                    if flow.submit_manual_code(pasted) {
                        self.notice("code submitted - completing the login...");
                    } else {
                        self.notice(
                            "this login cannot take a pasted code (device flow, \
                             or one already arrived)",
                        );
                    }
                }
                None => {
                    self.notice("no login waiting - start one with /login openai-codex");
                }
            },
        }
    }

    /// Swap in a fresh login flow; a previous attempt is cancelled by
    /// the drop (login.rs).
    fn start_login(&mut self, flow: login::LoginFlow, kind: &str) {
        if self.login.take().is_some() {
            self.notice("previous login attempt cancelled");
        }
        self.notice(format!(
            "starting the {kind} login for openai-codex... (esc cancels)"
        ));
        self.login = Some(flow);
    }

    /// `/logout` list stored logins, or remove one from auth.json.
    fn handle_logout_command(&mut self, args: &str) {
        let name = args.trim();
        if name.is_empty() {
            let stored = cupel_coding_agent::auth::load_auth(self.meta.home.as_deref());
            if stored.is_empty() {
                self.notice("no stored logins (auth.json is empty; /login adds one)");
            } else {
                let mut lines = vec!["stored logins (/logout <name>):".to_string()];
                for provider in stored.keys() {
                    lines.push(format!("  {provider}"));
                }
                self.notice(lines.join("\n"));
            }
            return;
        }
        match cupel_coding_agent::auth::delete_credential(self.meta.home.as_deref(), name) {
            Ok(true) => self.notice(format!("{name} logged out - credential removed")),
            Ok(false) => self.notice(format!("no stored login for {name} (/logout lists them)")),
            // SaveError never carries token values safe to show.
            Err(e) => self.notice(format!("logout failed: {e}")),
        }
    }

    /// A login event arrived (never blocks pure state + notices).
    pub fn on_login_event(&mut self, event: Option<login::LoginEvent>) {
        match event {
            Some(login::LoginEvent::Notice(text)) => self.notice(text),
            Some(login::LoginEvent::Progress(state, text)) => {
                if let Some(flow) = &mut self.login {
                    flow.on_progress(state);
                }
                self.notice(text);
            }
            Some(login::LoginEvent::Done(Ok(summary))) => {
                self.login = None;
                self.notice(format!(
                    "{summary}\nswitch with /provider openai-codex (default codex/gpt-6.1-sol)"
                ));
            }
            Some(login::LoginEvent::Done(Err(error))) => {
                self.login = None;
                self.notice(format!("login failed: {error}"));
            }
            // Channel closed without Done: the task was cancelled and its
            // final send raced the drop nothing to report.
            None => self.login = None,
        }
    }

    /// The `ask_user` tool asks. Until the answer, the question takes Up, Down and
    /// Enter (see `on_key`).
    pub fn on_question(&mut self, question: Option<PendingQuestion>) {
        let Some(pending) = question else {
            // Every sender is gone. Stop listening: a closed channel is always ready,
            // and the event loop would spin on it.
            self.questions = None;
            return;
        };
        if self.last_answer == Status::Cancelled {
            return;
        }
        let selected = pending.question().recommended();
        self.question = Some(OpenQuestion { pending, selected });
        // The question builds on what the agent just wrote: show the end.
        self.scroll_from_bottom = 0;
    }

    /// Await the next agent event, or park forever when no run is active.
    /// Parking (instead of returning None immediately) matters inside
    /// `tokio::select!`: a constantly-ready branch would busy-spin the loop.
    pub async fn next_agent_event(&mut self) -> Option<AgentEvent> {
        match &mut self.run_events {
            Some(events) => events.next().await,
            None => std::future::pending().await,
        }
    }

    /// The background sources (the run, a login, the questions), multiplexed
    /// behind one `&mut self` future mod.rs cannot hold two `app.next_...()`
    /// branches in its select! (each would borrow `app` mutably). Inside the
    /// method the borrows split field-by-field, which is exactly what the
    /// borrow checker is happy to prove.
    pub async fn next_event(&mut self) -> AppEvent {
        let run_events = &mut self.run_events;
        let login = &mut self.login;
        let questions = &mut self.questions;
        let usage_popup = &mut self.usage_popup;
        let helper = &mut self.helper;
        tokio::select! {
            event = async {
                match helper {
                    Some(run) => run.events.recv().await,
                    None => std::future::pending().await,
                }
            } => AppEvent::Helper(event),
            event = async {
                match run_events {
                    Some(events) => events.next().await,
                    None => std::future::pending().await,
                }
            } => AppEvent::Agent(event),
            event = async {
                match login {
                    Some(flow) => flow.next_event().await,
                    None => std::future::pending().await,
                }
            } => AppEvent::Login(event),
            question = async {
                match questions {
                    Some(questions) => questions.recv().await,
                    None => std::future::pending().await,
                }
            } => AppEvent::Question(question),
            result = async {
                match usage_popup {
                    Some(popup) => popup.next_event().await,
                    None => std::future::pending().await,
                }
            } => AppEvent::Usage(result)
        }
    }

    pub async fn on_event(&mut self, event: AppEvent) {
        match event {
            AppEvent::Agent(event) => self.on_agent_event(event).await,
            AppEvent::Login(event) => self.on_login_event(event),
            AppEvent::Helper(event) => self.on_helper_event(event),
            AppEvent::Question(question) => self.on_question(question),
            AppEvent::Usage(result) => {
                if let Some(popup) = &mut self.usage_popup {
                    popup.finish_loading(result);
                }
            }
        }
    }

    pub async fn on_agent_event(&mut self, event: Option<AgentEvent>) {
        let Some(event) = event else {
            // Stream closed without AgentEnd (shouldn't happen; be robust).
            if self.last_answer != Status::Cancelled {
                self.last_answer = Status::Failed;
            }
            self.finish_run().await;
            return;
        };

        match event {
            AgentEvent::MessageUpdate { event } => match event {
                AssistantMessageEvent::TextDelta { delta, .. } => {
                    self.activity = Activity::Writing;
                    self.transcript.append_assistant(&delta);
                }
                AssistantMessageEvent::ThinkingDelta { delta, .. } => {
                    self.activity = Activity::Thinking;
                    self.transcript.append_thinking(&delta);
                }
                AssistantMessageEvent::ToolCallStart { .. } => {
                    self.activity = Activity::CallingTool;
                }
                AssistantMessageEvent::ToolCallEnd { tool_call, .. } => {
                    let call = self
                        .agent
                        .describe_tool_call(&tool_call.name, &tool_call.arguments);
                    self.transcript.cells.push(Cell::Tool {
                        id: tool_call.id,
                        name: tool_call.name,
                        call,
                        expanded: self.tools_expanded,
                        started_at: None,
                        live: None,
                        result: None,
                    });
                }
                _ => {}
            },
            // Every finalized message arrives here, user prompts included:
            // the run's own prompt and each queued prompt at the moment the
            // model receives it.
            AgentEvent::MessageEnd { message } => {
                // Every finalized message rides into the transcript file.
                self.recorder.record(&message);
                if let AgentMessage::Llm(Message::Assistant(assistant)) = &message {
                    self.account_assistant(assistant);
                } else if let AgentMessage::Llm(Message::User(user)) = &message
                    && let UserContentBody::Text(text) = &user.content
                    && let Some(index) = self.queued.iter().position(|queued| queued == text)
                {
                    // A queued prompt reached the model. The turn before it is complete,
                    // so its last prose becomes its answer.
                    self.queued.remove(index);
                    self.transcript.promote_final_answer();
                    self.transcript
                        .cells
                        .push(Cell::User { text: text.clone() });
                } else if let AgentMessage::Llm(Message::User(user)) = &message
                    && let UserContentBody::Text(text) = &user.content
                    && let Some(index) = self.steering.iter().position(|steered| steered == text)
                {
                    // A steered prompt reached the model. Unlike a queued one it
                    // arrives mid-task, so the prose before it is no final answer and
                    // stays as it is.
                    self.steering.remove(index);
                    self.transcript
                        .cells
                        .push(Cell::User { text: text.clone() });
                }
            }
            AgentEvent::ToolExecutionStart {
                tool_call_id,
                tool_name,
            } => {
                self.activity = Activity::Running(tool_name);
                self.transcript.mark_tool_started(&tool_call_id);
            }
            AgentEvent::ToolExecutionUpdate {
                tool_call_id,
                partial,
                ..
            } => {
                let snapshot = tool_outcome(&partial.content, None, false).text;
                self.transcript
                    .attach_tool_progress(&tool_call_id, snapshot);
            }
            AgentEvent::ToolExecutionEnd {
                tool_call_id,
                result,
                is_error,
                ..
            } => {
                self.transcript.attach_tool_result(
                    &tool_call_id,
                    tool_outcome(&result.content, result.details.as_ref(), is_error),
                );
            }
            AgentEvent::CompactionStart { reason } => {
                self.activity = Activity::Compacting;
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
                summary,
                ..
            } => {
                let text = match error {
                    None => {
                        self.context_tokens = tokens_after;
                        format!(
                            "context compacted: ~{}k -> ~{}k tokens",
                            tokens_before / 1000,
                            tokens_after / 1000,
                        )
                    }
                    Some(error) => format!("compaction failed: {error}"),
                };
                self.transcript.cells.push(Cell::Notice { text });
                if let Some(summary) = summary {
                    self.transcript.cells.push(Cell::Summary { text: summary });
                }
            }
            AgentEvent::AutoRetry {
                attempt,
                max_attempts,
                delay_ms,
                error_message,
            } => {
                self.activity = Activity::Retrying;
                self.transcript.cells.push(Cell::Notice {
                    text: format!(
                        "retrying in {:.1}s (attempt {attempt}/{max_attempts}): {error_message}",
                        delay_ms as f64 / 1000.0
                    ),
                });
            }
            AgentEvent::AgentEnd { .. } => self.finish_run().await,
            AgentEvent::TurnStart => self.activity = Activity::Waiting,
            AgentEvent::TurnEnd { .. } | AgentEvent::ThinkingBlocksRemoved { .. } => {}
        }
    }

    async fn finish_run(&mut self) {
        if self.last_answer == Status::New {
            self.last_answer = Status::Failed;
        }
        // The trailing assistant prose was the run's fnal answer
        // promote it so the turn visibly ends with its results.
        self.transcript.promote_final_answer();
        // How long the run took, below its last answer.
        if let Some(started) = self.run_started.take() {
            self.transcript.cells.push(Cell::Usage {
                text: format!("worked for {}", format_elapsed(started.elapsed())),
            });
        }
        self.run_events = None;
        // A run can end with its question still open (a crash, a closed event stream).
        // A question nobody waits for must not keep taking Enter.
        self.question = None;
        // Fire the `stop` hook without blocking the UI; the next prompt's
        // before_prompt (or session end) settles it.
        self.recorder.on_agent_end();
        // Joins the (already finished) run tasks so state flags settle.
        self.agent.wait_for_idle().await;
        self.run_finished = true;
        self.record_helper_spend();
        self.start_waiting();
    }

    fn start_waiting(&mut self) {
        if let Some(call) = self.waiting_helper.take() {
            self.start_helper(call);
            return;
        }
        // A waiting prompt can outlive its run. Submitted after the loop's last look at
        // the queues, or waiting behind a run that failed. Steered prompts go first,
        // all together, as they would have in the run.
        let steering = self.agent.take_steering();
        let next = if steering.is_empty() {
            self.agent.take_follow_up().map(|message| vec![message])
        } else {
            Some(steering)
        };
        if let Some(messages) = next {
            match self.agent.prompt(messages) {
                Ok(events) => self.begin_run(events),
                Err(err) => {
                    self.last_answer = Status::Failed;
                    self.transcript.cells.push(Cell::Error {
                        text: err.to_string(),
                    });
                }
            }
        }
    }
}

/// The last headline in a reasoning text: a line that is bold as a whole,
/// `**Like this**`. Reasoning summaries open each section with one; raw
/// thinking has none.
#[must_use]
pub fn reasoning_headline(text: &str) -> Option<&str> {
    text.lines()
        .rev()
        .map(str::trim)
        .find_map(|line| line.strip_prefix("**")?.strip_suffix("**"))
        .filter(|headline| !headline.is_empty())
}

/// A duration the way a clock reads it: `42s`, `1m 05s`.
#[must_use]
pub fn format_elapsed(elapsed: Duration) -> String {
    let seconds = elapsed.as_secs();
    if seconds < 60 {
        format!("{seconds}s")
    } else {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    }
}

/// One line per preset for the `/preset` popup and listening, e.g.
/// `anthropic/claude-sonnet-5`, thinking high, + prompt`.
fn preset_summary(preset: &Preset) -> String {
    let prompt = if preset.prompt.as_deref().is_some_and(|p| !p.is_empty()) {
        ", + prompt"
    } else {
        ""
    };
    format!(
        "{}/{}, thinking {}{prompt}",
        preset.provider,
        preset.model,
        preset.thinking_level.as_str(),
    )
}

/// Session-id completion candidates: transcript file stems, newest first
/// by modification time. Deliberately does not parse the transcripts
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

/// The transcript's view of a finished tool call: the text the model saw,
/// plus the diff an edit ships in its `details`. Shared by the live path
/// (ToolExecutionEnd) and the --resume replay, so both show the same thing.
fn tool_outcome(
    content: &[ToolResultContent],
    details: Option<&serde_json::Value>,
    is_error: bool,
) -> ToolOutcome {
    let text: String = content
        .iter()
        .filter_map(|c| match c {
            ToolResultContent::Text(t) => Some(t.text.as_str()),
            ToolResultContent::Image(_) => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    let diff = details
        .and_then(|d| d.get("diff"))
        .and_then(serde_json::Value::as_str)
        .map(str::to_string);
    ToolOutcome {
        text,
        is_error,
        diff,
        took: None,
    }
}
