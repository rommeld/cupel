//! The render pass: `App` state in, one frame out.
//!
//! ratatui is immediate mode - this function redescribes the ENTIRE screen
//! every frame, and the library diffs against the previous frame to emit
//! minimal terminal writes. So there is no "update the widget" anywhere;
//! there is only state (in `App`) and this projection of it.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Margin, Position, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Clear, Padding, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
};

use crate::modes::interactive::{app::App, theme, transcript};

pub fn render(frame: &mut Frame<'_>, app: &mut App) {
    // Input grows with its content (explicit newlines + wrapped lines),
    // capped at 5 visible lines, + 2 border rows. The inner width is the
    // full frame width minus the left/right borders.
    let inner_width = frame.area().width.saturating_sub(2).max(1) as usize;
    let input_lines = app
        .input
        .text()
        .split('\n')
        .map(|line| transcript::wrap_line(line, inner_width).len())
        .sum::<usize>()
        .clamp(1, 5) as u16;
    let [transcript_area, input_area, footer_area] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(input_lines + 2),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    render_transcript(frame, app, transcript_area);
    render_input(frame, app, input_area);
    render_footer(frame, app, footer_area);
    // Drawn LAST so it overdraws the transcript's bottom rows - in
    // immediate-mode rendering, paint order IS the z-order.
    render_autocomplete(frame, app, transcript_area, input_area);
}

/// The `@path` completion popup, anchored just above the input box at the
/// column of the token's `@`.
fn render_autocomplete(frame: &mut Frame<'_>, app: &App, transcript_area: Rect, input_area: Rect) {
    let Some((rows, selected)) = app.autocomplete.visible() else {
        return;
    };
    if transcript_area.height == 0 {
        return;
    }

    let height = (rows.len() as u16).min(transcript_area.height);
    // Anchor x to the `@` column when it's on the input's visible first
    // line; degrade gracefully to the input's left edge otherwise.
    let anchor_col = app.autocomplete.token_start().map_or(0, |start| {
        app.input.text()[..]
            .chars()
            .take(start)
            .filter(|c| *c != '\n')
            .count() as u16
    });
    let width = rows
        .iter()
        .map(|r| r.display.len() as u16 + 2)
        .max()
        .unwrap_or(10)
        .min(frame.area().width);
    let x = (input_area.x + 1 + anchor_col).min(frame.area().width.saturating_sub(width));

    let popup = Rect {
        x,
        y: input_area.y.saturating_sub(height),
        width,
        height,
    };

    // Clear blanks the transcript underneath, then the rows paint on top.
    frame.render_widget(Clear, popup);
    let lines: Vec<Line<'_>> = rows
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let style = if i == selected {
                theme::POPUP_SELECTED
            } else {
                theme::POPUP_ROW
            };
            Line::from(Span::styled(format!(" {} ", row.display), style))
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), popup);
}

fn render_transcript(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    // The tools pane exists only once there is tool traffic; a pure chat
    // keeps the full width for prose.
    let has_tools = app
        .transcript
        .cells
        .iter()
        .any(|cell| matches!(cell, transcript::Cell::Tool { .. }));
    let (chat_area, tools_area) = if has_tools {
        let [chat, tools] = Layout::horizontal([
            Constraint::Percentage(CHAT_PANE_PERCENT),
            Constraint::Percentage(100 - CHAT_PANE_PERCENT),
        ])
        .areas(area);
        (chat, Some(tools))
    } else {
        (area, None)
    };

    // Blocks render first, content after - `inner` subtracts borders AND
    // padding, so the Paragraphs below never touch the chrome.
    let chat_block = pane_block(" conversation ");
    let chat_inner = chat_block.inner(chat_area);
    frame.render_widget(chat_block, chat_area);
    let tools_inner = tools_area.map(|tools_area| {
        let block = pane_block(" tools ");
        let inner = block.inner(tools_area);
        frame.render_widget(block, tools_area);
        inner
    });

    let columns = app.transcript.to_columns(
        chat_inner.width,
        tools_inner.map_or(0, |inner| inner.width),
        app.selected_cell,
    );
    let total = columns.left.len();
    let height = chat_inner.height as usize;

    // Remember geometry so key/mouse handlers can clamp and hit-test.
    app.last_total_lines = total;
    app.last_transcript_height = chat_inner.height;
    let max_scroll = total.saturating_sub(height);
    app.scroll_from_bottom = app.scroll_from_bottom.min(max_scroll);

    // Bottom-anchored window: offset 0 shows the newest lines. The SAME
    // window slices both panes - that lockstep is what keeps a band's
    // reasoning and tool calls on the same rows while scrolling.
    let end = total - app.scroll_from_bottom;
    let start = end.saturating_sub(height);
    app.last_top_line = start;
    app.last_chat_inner = chat_inner;
    app.last_line_cells = columns.cell_at;

    frame.render_widget(
        Paragraph::new(columns.left[start..end].to_vec()),
        chat_inner,
    );
    if let Some(tools_inner) = tools_inner {
        frame.render_widget(
            Paragraph::new(columns.right[start..end].to_vec()),
            tools_inner,
        );
    }

    // ONE scrollbar for the lockstep panes, on the transcript's right
    // edge; the vertical margin spares the border corner glyphs. Rendered
    // only when there is something to scroll - a permanently full bar
    // would read as decoration.
    if total > height {
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .track_symbol(Some("│"))
            .track_style(theme::PANE_BORDER)
            .thumb_style(theme::SCROLLBAR_THUMB);
        let mut state = ScrollbarState::new(total).position(start);
        frame.render_stateful_widget(
            scrollbar,
            area.inner(Margin {
                vertical: 1,
                horizontal: 0,
            }),
            &mut state,
        );
    }

    // A scroll indicator only when not following the tail.
    if app.scroll_from_bottom > 0 {
        let marker = format!(" ↓ {} more ", app.scroll_from_bottom);
        let width = marker.len() as u16;
        let marker_area = Rect {
            x: area.right().saturating_sub(width + 1),
            y: area.bottom().saturating_sub(1),
            width: width.min(area.width),
            height: 1,
        };
        frame.render_widget(
            Paragraph::new(marker).style(theme::SCROLL_MARKER),
            marker_area,
        );
    }
}

/// Left pane share of the transcript width: prose needs more room than
/// tool call previews.
const CHAT_PANE_PERCENT: u16 = 60;

/// The shared look of both transcript panes: dim border, dim title, one
/// column of padding so text never sticks to a border line.
fn pane_block(title: &'static str) -> Block<'static> {
    Block::new()
        .borders(Borders::ALL)
        .border_style(theme::PANE_BORDER)
        .title(Span::styled(title, theme::CHROME))
        .padding(Padding::horizontal(1))
}

/// Cursor position as (visual line, visual column) in the wrapped input
/// text. Derived from the SAME `wrap_line` output that renders the text: a
/// second, parallel wrapping computation would inevitably disagree with it
/// (word wrap vs. plain column wrap) and paint the cursor away from where
/// the next keystroke actually lands.
///
/// `cursor` is a CHAR index (see `InputState`). `wrap_line` preserves every
/// character of its input across the chunks it returns, so char offsets map
/// 1:1 onto the wrapped output and locating the cursor is just counting.
fn visual_cursor(text: &str, cursor: usize, width: usize) -> (usize, usize) {
    use unicode_width::UnicodeWidthChar;

    let mut remaining = cursor; // chars between the start of `text` and the cursor
    let mut visual_line = 0;
    for logical in text.split('\n') {
        let chunks = transcript::wrap_line(logical, width);
        let line_chars = logical.chars().count();
        if remaining <= line_chars {
            // The cursor sits on this logical line: walk its wrapped chunks
            // until the offset falls inside one.
            for (i, chunk) in chunks.iter().enumerate() {
                let chunk_chars = chunk.chars().count();
                // Landing exactly on a chunk boundary means "before the
                // first char of the NEXT chunk" - inserting there joins the
                // next chunk's word, so that is where the char will appear.
                // Only at the very end of the line does the cursor trail
                // the last chunk instead.
                if remaining < chunk_chars || i + 1 == chunks.len() {
                    let col = chunk
                        .chars()
                        .take(remaining)
                        .map(|c| c.width().unwrap_or(0))
                        .sum();
                    return (visual_line + i, col);
                }
                remaining -= chunk_chars;
            }
        }
        remaining -= line_chars + 1; // +1 consumes the '\n'
        visual_line += chunks.len();
    }
    (visual_line, 0) // unreachable: the last logical line always contains the cursor
}

fn render_input(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let border_style = if app.is_running() {
        theme::INPUT_BORDER_BUSY
    } else {
        theme::INPUT_BORDER_IDLE
    };
    let title = if app.is_running() {
        " working "
    } else {
        " prompt "
    };
    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(Span::styled(title, theme::CHROME));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let inner_width = inner.width.max(1) as usize;
    let text: Vec<Line<'_>> = app
        .input
        .text()
        .split('\n')
        .flat_map(|line| transcript::wrap_line(line, inner_width))
        .map(Line::from)
        .collect();

    // Scroll the viewport so the cursor's line stays visible once the text
    // outgrows the height-capped box - otherwise the user would be typing
    // into rows that render off-screen.
    let (cursor_line, cursor_col) =
        visual_cursor(app.input.text(), app.input.cursor(), inner_width);
    let visible = inner.height.max(1) as usize;
    let scroll = cursor_line.saturating_sub(visible - 1);
    frame.render_widget(Paragraph::new(text).scroll((scroll as u16, 0)), inner);

    // Place the real terminal cursor at the editing position. (ratatui hides
    // it unless the app explicitly positions it each frame.)
    frame.set_cursor_position(Position {
        x: inner.x + (cursor_col as u16).min(inner.width.saturating_sub(1)),
        y: inner.y + ((cursor_line - scroll) as u16).min(inner.height.saturating_sub(1)),
    });
}

fn render_footer(frame: &mut Frame<'_>, app: &App, area: Rect) {
    let state = if app.is_running() { "working" } else { "idle" };
    // the thinking level appears only for
    // reasoning models; "off" keeps the word "thinking" so a bare level
    // name never reads like part of the model name.
    let thinking_segment = if app.agent.model_supports_reasoning() {
        match app.agent.thinking_level() {
            None => " | thinking off".to_string(),
            Some(level) => format!(" | thinking {}", thinking_level_name(level)),
        }
    } else {
        String::new()
    };
    // The session id sits in the always-visible left half so `--resume
    // <id>` (or /hot-reload <id>) can be typed from what's on screen.
    let left = format!(
        " {} ({}){} | {} | {} | {} in / {} out / {} cached | ${:.4}",
        app.meta.model_name,
        app.meta.provider,
        thinking_segment,
        app.recorder.session_id(),
        state,
        app.totals.input,
        app.totals.output,
        app.totals.cache_read,
        app.totals.cost,
    );
    // The mouse hint tracks selection mode, so it never lies about what
    // the wheel currently does.
    let right = if app.mouse_captured {
        "enter send · alt+enter newline · @ file · / cmds · esc abort · click block · ctrl+o copy · ctrl+y select "
    } else {
        "enter send · alt+enter newline · @ file · / cmds · esc abort · SELECTION MODE · ctrl+y scroll "
    };

    // Left-align the status, right-align the key hints; drop the hints when
    // the terminal is too narrow for both. Chars, not bytes: every `·` in
    // the hints is 2 bytes of UTF-8 but only 1 terminal column - len()
    // would overestimate and drop the hints while they still fit.
    let mut spans = vec![Span::styled(left.clone(), theme::CHROME)];
    let padding =
        (area.width as usize).saturating_sub(left.chars().count() + right.chars().count());
    if padding > 0 {
        spans.push(Span::raw(" ".repeat(padding)));
        spans.push(Span::styled(right, theme::CHROME));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Display name for a thinking level - the same lowercase words the
/// /thinking command accepts, so footer and command speak one language.
fn thinking_level_name(level: cupel_core::types::ThinkingLevel) -> &'static str {
    use cupel_core::types::ThinkingLevel;
    match level {
        ThinkingLevel::Minimal => "minimal",
        ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        ThinkingLevel::High => "high",
        ThinkingLevel::XHigh => "xhigh",
    }
}

#[cfg(test)]
mod tests {
    //! Headless render tests: ratatui's `TestBackend` draws frames into an
    //! in-memory buffer, so the full render path is testable without a
    //! terminal (or an API key - the Agent is constructed but never run).

    use super::*;
    use crate::modes::SessionMeta;
    use crate::modes::interactive::transcript::{Cell, ToolOutcome};
    use cupel_agent::{Agent, AgentEvent, AgentOptions};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use ratatui::style::Color;
    use ratatui::style::Modifier;
    use std::sync::Arc;

    fn test_app() -> App {
        test_app_in("/tmp")
    }

    /// App rooted at a specific cwd - the autocomplete tests point this at
    /// a temp tree with known files.
    fn test_app_in(cwd: &str) -> App {
        let model = cupel_core::catalog::builtin_models().remove(0);
        let registry = Arc::new(cupel_core::provider::Registry::new());
        let agent = Agent::new(AgentOptions::new(model, registry));
        // home: None disables persistence + hooks - tests touch no disk.
        let recorder = crate::session::SessionRecorder::new(
            None,
            std::path::Path::new(cwd),
            "cupel-test",
            "test-model",
        );
        App::new(
            agent,
            SessionMeta {
                model_name: "Test Model".into(),
                provider: "test".into(),
                cwd: cwd.into(),
                templates: Vec::new(),
                // Real builtin catalog so /model and /provider tests exercise
                // the same data the app ships with.
                models: cupel_core::catalog::builtin_models(),
                settings: crate::settings::Settings::default(),
                home: None,
                startup_warning: None,
                context_files: Vec::new(),
            },
            recorder,
        )
    }

    /// A temp project for autocomplete render tests.
    fn autocomplete_cwd(name: &str) -> String {
        let root = std::env::temp_dir().join(format!("cupel-ui-autocomplete-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() {}").unwrap();
        std::fs::write(root.join("notes.md"), "# notes").unwrap();
        root.display().to_string()
    }

    fn type_text(app: &mut App, text: &str) {
        for c in text.chars() {
            app.on_terminal_event(Event::Key(KeyEvent::new(
                KeyCode::Char(c),
                KeyModifiers::NONE,
            )));
        }
    }

    fn draw(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| render(frame, app)).unwrap();
        // Flatten the buffer to a string for containment assertions.
        let buffer = terminal.backend().buffer().clone();
        let mut out = String::new();
        for y in 0..buffer.area.height {
            for x in 0..buffer.area.width {
                out.push_str(buffer[(x, y)].symbol());
            }
            out.push('\n');
        }
        out
    }

    /// Draw the app and return the style painted at the first occurrence
    /// of `needle`. ASCII needles only: the byte index of the match is
    /// then also its column. Panics when the needle is not on screen -
    /// that is a failed test either way, and panicking here gives the
    /// missing-text message instead of a confusing style mismatch.
    fn style_of(app: &mut App, needle: &str) -> ratatui::style::Style {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| render(frame, app)).unwrap();
        let buffer = terminal.backend().buffer().clone();
        for y in 0..buffer.area.height {
            let mut row = String::new();
            for x in 0..buffer.area.width {
                row.push_str(buffer[(x, y)].symbol());
            }
            if let Some(col) = row.find(needle) {
                return buffer[(u16::try_from(col).unwrap(), y)].style();
            }
        }
        panic!("needle {needle:?} not on screen");
    }

    #[test]
    fn the_turn_hierarchy_reaches_the_screen() {
        let mut app = test_app();
        app.transcript.cells.push(Cell::User {
            text: "the task".into(),
        });
        app.transcript.cells.push(Cell::Thinking {
            text: "pondering".into(),
        });
        app.transcript.cells.push(Cell::Answer {
            text: "the answer".into(),
        });

        let task = style_of(&mut app, "> the task");
        assert_eq!(task.fg, Some(Color::LightGreen));

        assert_eq!(style_of(&mut app, "pondering").fg, Some(Color::DarkGray));

        let answer = style_of(&mut app, "the answer");
        assert_eq!(answer.fg, Some(Color::Magenta));
    }

    #[test]
    fn startup_warning_leads_the_transcript_as_a_notice() {
        let model = cupel_core::catalog::builtin_models().remove(0);
        let registry = Arc::new(cupel_core::provider::Registry::new());
        let recorder = crate::session::SessionRecorder::new(
            None,
            std::path::Path::new("/tmp"),
            "cupel-test",
            "test-model",
        );
        let mut app = App::new(
            Agent::new(AgentOptions::new(model, registry)),
            SessionMeta {
                model_name: "Test Model".into(),
                provider: "test".into(),
                cwd: "/tmp".into(),
                templates: Vec::new(),
                models: cupel_core::catalog::builtin_models(),
                settings: crate::settings::Settings::default(),
                home: None,
                startup_warning: Some(
                    "no credentials found - use /provider <name> <api-key>".into(),
                ),
                context_files: Vec::new(),
            },
            recorder,
        );
        // First cell is the warning notice - the session is usable, not
        // blocked.
        assert!(matches!(
            &app.transcript.cells[0],
            Cell::Notice { text } if text.contains("no credentials found")
        ));
        let screen = draw(&mut app, 100, 20);
        assert!(
            screen.contains("no credentials found"),
            "warning must render:\n{screen}"
        );
        // Typing and submitting still works (the prompt is queued).
        type_text(&mut app, "hello");
        app.on_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
        assert!(app.pending_prompt.is_some(), "session accepts prompts");
    }

    #[test]
    fn review_command_queues_a_bundled_prompt_or_notices_errors() {
        let root = std::env::temp_dir().join("cupel-ui-review");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("code.rs"), "fn main() {}").unwrap();
        let mut app = test_app_in(root.to_str().unwrap());

        // A reviewable path: the built bundle is queued as the prompt.
        type_text(&mut app, "/review code.rs");
        app.on_terminal_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
        app.on_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
        let prompt = app.pending_prompt.take().expect("review prompt queued");
        assert!(prompt.contains("=== file: code.rs ==="));
        assert!(prompt.contains("fn main() {}"));

        // A bad path: error notice, nothing queued or sent.
        type_text(&mut app, "/review missing.rs");
        app.on_terminal_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
        app.on_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
        assert!(app.pending_prompt.is_none());
        assert!(app.transcript.cells.iter().any(|c| matches!(
            c,
            Cell::Notice { text } if text.contains("path not found: missing.rs")
        )));
    }

    #[test]
    fn footer_shows_the_current_session_id() {
        let mut app = test_app();
        let screen = draw(&mut app, 100, 20);
        assert!(
            screen.contains("cupel-test"),
            "session id missing from footer:\n{screen}"
        );
    }

    #[test]
    fn session_id_command_lists_this_projects_sessions() {
        // A home-backed recorder (unlike test_app's disabled one) so the
        // listing has a real sessions dir to read.
        let root = std::env::temp_dir().join("cupel-ui-session-list");
        let _ = std::fs::remove_dir_all(&root);
        let (home, cwd) = (root.join("home"), root.join("proj"));
        std::fs::create_dir_all(&cwd).unwrap();
        let recorder =
            crate::session::SessionRecorder::new(Some(home), &cwd, "cupel-current", "test-model");
        // Pre-write an OLDER session the listing must show alongside.
        let dir = recorder.sessions_dir().unwrap().to_path_buf();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("cupel-old.jsonl"),
            format!(
                "{}\n{}\n",
                serde_json::json!({"version": 1, "sessionId": "cupel-old", "cwd": cwd.display().to_string(), "model": "test-model", "startedAt": 1_000}),
                serde_json::to_string(&cupel_agent::AgentMessage::user_text("find the bug")).unwrap(),
            ),
        )
        .unwrap();

        let model = cupel_core::catalog::builtin_models().remove(0);
        let registry = Arc::new(cupel_core::provider::Registry::new());
        let mut app = App::new(
            Agent::new(AgentOptions::new(model, registry)),
            SessionMeta {
                model_name: "Test Model".into(),
                provider: "test".into(),
                cwd: cwd.display().to_string(),
                templates: Vec::new(),
                models: cupel_core::catalog::builtin_models(),
                settings: crate::settings::Settings::default(),
                home: None,
                startup_warning: None,
                context_files: Vec::new(),
            },
            recorder,
        );

        type_text(&mut app, "/session-id");
        app.on_terminal_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
        app.on_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));

        let notice = app
            .transcript
            .cells
            .iter()
            .find_map(|c| match c {
                Cell::Notice { text } => Some(text.clone()),
                _ => None,
            })
            .expect("listing notice");
        assert!(
            notice.contains("current session: cupel-current"),
            "{notice}"
        );
        assert!(notice.contains("cupel-old"), "{notice}");
        assert!(notice.contains("find the bug"), "label = first prompt");
        assert!(notice.contains("1970-01-01"), "startedAt 1000ms date");
    }

    /// App with a REAL home + cwd on disk, for reload/resume tests.
    fn test_app_with_home(root: &std::path::Path, session_id: &str) -> App {
        let (home, cwd) = (root.join("home"), root.join("proj"));
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        let recorder = crate::session::SessionRecorder::new(
            Some(home.clone()),
            &cwd,
            session_id,
            "test-model",
        );
        let model = cupel_core::catalog::builtin_models().remove(0);
        let registry = Arc::new(cupel_core::provider::Registry::new());
        App::new(
            Agent::new(AgentOptions::new(model, registry)),
            SessionMeta {
                model_name: "Test Model".into(),
                provider: "test".into(),
                cwd: cwd.display().to_string(),
                templates: Vec::new(),
                models: cupel_core::catalog::builtin_models(),
                settings: crate::settings::Settings::default(),
                home: Some(home),
                startup_warning: None,
                context_files: Vec::new(),
            },
            recorder,
        )
    }

    #[test]
    fn hot_reload_command_sets_the_pending_target() {
        let mut app = test_app();
        type_text(&mut app, "/hot-reload");
        app.on_terminal_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
        app.on_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
        assert_eq!(
            app.pending_reload,
            Some(crate::modes::interactive::app::ReloadTarget::Current)
        );

        let mut app = test_app();
        type_text(&mut app, "/hot-reload cupel-42");
        app.on_terminal_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
        app.on_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
        assert_eq!(
            app.pending_reload,
            Some(crate::modes::interactive::app::ReloadTarget::Resume(
                "cupel-42".into()
            ))
        );
    }

    #[tokio::test]
    async fn hot_reload_current_appends_only_the_agents_delta() {
        use crate::modes::interactive::app::ReloadTarget;
        use crate::resources::{CONTEXT_UPDATE_MARKER, ContextFile};

        let root = std::env::temp_dir().join("cupel-ui-hotreload-delta");
        let _ = std::fs::remove_dir_all(&root);
        // Ten rules: far enough apart that the unified diff's 2-line
        // context radius provably excludes the untouched start of the file.
        let original: String = (1..=10).fold(String::new(), |mut s, i| {
            use std::fmt::Write as _;
            let _ = writeln!(s, "RULE {i}");
            s
        });
        let mut app = test_app_with_home(&root, "cupel-current");
        std::fs::write(root.join("home/AGENTS.md"), &original).unwrap();
        // The session-start baseline the delta will diff against.
        app.meta.context_files = vec![ContextFile {
            path: root.join("home/AGENTS.md"),
            content: original.clone(),
        }];
        // Some history that must SURVIVE the reload.
        app.agent = {
            let model = cupel_core::catalog::builtin_models().remove(0);
            let registry = Arc::new(cupel_core::provider::Registry::new());
            let mut options = AgentOptions::new(model, registry);
            options.messages = vec![cupel_agent::AgentMessage::user_text("earlier prompt")];
            Agent::new(options)
        };

        // Edit ONE rule mid-session.
        let edited = original.replace("RULE 6", "RULE 6 (amended)");
        std::fs::write(root.join("home/AGENTS.md"), &edited).unwrap();

        let app = app.hot_reload(ReloadTarget::Current).await;

        // The session CONTINUES: same id, history intact.
        assert_eq!(app.recorder.session_id(), "cupel-current");
        let messages = app.agent.state().messages;
        assert_eq!(messages.len(), 2, "history + appended delta");
        // The system prompt was NOT rebuilt (test agent starts with an
        // empty one - re-embedding would have injected the rules).
        assert!(!app.agent.state().system_prompt.contains("RULE"));
        // The appended message is the DELTA, not the whole file: the
        // changed line travels, distant unchanged lines do not.
        let cupel_agent::AgentMessage::Llm(cupel_core::types::Message::User(user)) = &messages[1]
        else {
            panic!("delta should be a user message");
        };
        let cupel_core::types::UserContentBody::Text(delta) = &user.content else {
            panic!("delta should be text");
        };
        assert!(delta.starts_with(CONTEXT_UPDATE_MARKER));
        assert!(delta.contains("+RULE 6 (amended)"), "{delta}");
        assert!(delta.contains("-RULE 6\n"), "{delta}");
        assert!(
            !delta.contains("RULE 1\n"),
            "far lines must not travel: {delta}"
        );
        assert!(
            app.transcript.cells.iter().any(|c| matches!(
                c,
                Cell::Notice { text } if text.contains("reloaded in place")
            )),
            "in-place notice shown"
        );

        // Reload again with NO further edits: nothing new is appended.
        let app = app.hot_reload(ReloadTarget::Current).await;
        assert_eq!(app.agent.state().messages.len(), 2, "no duplicate delta");
        assert!(app.transcript.cells.iter().any(|c| matches!(
            c,
            Cell::Notice { text } if text.contains("no context file changes")
        )));
    }

    #[tokio::test]
    async fn hot_reload_resumes_a_session_by_id_and_rejects_unknown_ids() {
        use crate::modes::interactive::app::ReloadTarget;

        let root = std::env::temp_dir().join("cupel-ui-hotreload-resume");
        let _ = std::fs::remove_dir_all(&root);
        let app = test_app_with_home(&root, "cupel-current");
        // A finished older session on disk.
        let dir = app.recorder.sessions_dir().unwrap().to_path_buf();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("cupel-old.jsonl"),
            format!(
                "{}\n{}\n",
                serde_json::json!({"version": 1, "sessionId": "cupel-old", "cwd": "x", "model": "test-model", "startedAt": 1_000}),
                serde_json::to_string(&cupel_agent::AgentMessage::user_text("old prompt")).unwrap(),
            ),
        )
        .unwrap();

        // Unknown id: old app comes back intact, with an error notice.
        let app = app
            .hot_reload(ReloadTarget::Resume("cupel-nope".into()))
            .await;
        assert_eq!(app.recorder.session_id(), "cupel-current");
        assert!(app.transcript.cells.iter().any(|c| matches!(
            c,
            Cell::Notice { text } if text.contains("no session named cupel-nope")
        )));

        // Known id: history seeded, cells replayed, same id continued.
        let app = app
            .hot_reload(ReloadTarget::Resume("cupel-old".into()))
            .await;
        assert_eq!(app.recorder.session_id(), "cupel-old");
        assert_eq!(app.agent.state().messages.len(), 1);
        assert!(app.transcript.cells.iter().any(|c| matches!(
            c,
            Cell::User { text } if text == "old prompt"
        )));
    }

    #[test]
    fn renders_transcript_input_and_footer() {
        let mut app = test_app();
        app.transcript.cells.push(Cell::User {
            text: "find the bug".into(),
        });
        app.transcript.cells.push(Cell::Tool {
            id: "call_1".into(),
            name: "grep".into(),
            args: r#"{"pattern":"bug"}"#.into(),
            result: Some(ToolOutcome {
                text: "src/main.rs:1: bug".into(),
                is_error: false,
            }),
        });
        app.input.insert_str("next question");

        let screen = draw(&mut app, 80, 20);
        assert!(
            screen.contains("> find the bug"),
            "user cell missing:\n{screen}"
        );
        assert!(screen.contains("[grep]"), "tool cell missing:\n{screen}");
        assert!(
            screen.contains("src/main.rs:1: bug"),
            "tool result missing:\n{screen}"
        );
        assert!(screen.contains("next question"), "input missing:\n{screen}");
        assert!(screen.contains("Test Model"), "footer missing:\n{screen}");
    }

    #[test]
    fn typing_updates_input_and_enter_when_empty_is_a_noop() {
        let mut app = test_app();
        for c in "hi".chars() {
            app.on_terminal_event(Event::Key(KeyEvent::new(
                KeyCode::Char(c),
                KeyModifiers::NONE,
            )));
        }
        assert_eq!(app.input.text(), "hi");

        // Alt+Enter inserts a newline instead of submitting.
        app.on_terminal_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT)));
        assert_eq!(app.input.text(), "hi\n");
    }

    #[test]
    fn scroll_clamps_to_content() {
        let mut app = test_app();
        for i in 0..50 {
            app.transcript.cells.push(Cell::Assistant {
                text: format!("line {i}"),
            });
        }
        // Render once so the app learns the viewport geometry.
        let _ = draw(&mut app, 40, 10);
        // Scroll way past the top: must clamp, not underflow or overshoot.
        for _ in 0..100 {
            app.on_terminal_event(Event::Key(KeyEvent::new(
                KeyCode::PageUp,
                KeyModifiers::NONE,
            )));
        }
        let _ = draw(&mut app, 40, 10);
        assert!(app.scroll_from_bottom <= app.last_total_lines);
        // And back down to following.
        for _ in 0..100 {
            app.on_terminal_event(Event::Key(KeyEvent::new(
                KeyCode::PageDown,
                KeyModifiers::NONE,
            )));
        }
        assert_eq!(app.scroll_from_bottom, 0);
    }

    #[test]
    fn ctrl_c_quits_when_idle() {
        let mut app = test_app();
        app.on_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL,
        )));
        assert!(app.should_quit);
    }

    #[test]
    fn typing_at_query_renders_popup_above_input() {
        let mut app = test_app_in(&autocomplete_cwd("popup"));
        type_text(&mut app, "@ma");
        assert!(app.autocomplete.is_open());
        let screen = draw(&mut app, 80, 20);
        assert!(screen.contains("src/main.rs"), "popup missing:\n{screen}");
    }

    #[test]
    fn enter_with_popup_open_inserts_instead_of_submitting() {
        let mut app = test_app_in(&autocomplete_cwd("enter"));
        type_text(&mut app, "@ma");
        app.on_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
        // The reference is now in the buffer; nothing was submitted (no run
        // started, no user cell in the transcript).
        assert_eq!(app.input.text(), "@src/main.rs ");
        assert!(!app.is_running());
        assert!(app.transcript.cells.is_empty());
        // The completed file token closed the popup (trailing space).
        assert!(!app.autocomplete.is_open());
    }

    #[test]
    fn esc_with_popup_open_closes_popup_not_the_app() {
        let mut app = test_app_in(&autocomplete_cwd("esc"));
        type_text(&mut app, "@ma");
        assert!(app.autocomplete.is_open());
        app.on_terminal_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
        assert!(!app.autocomplete.is_open());
        assert!(!app.should_quit);
        let screen = draw(&mut app, 80, 20);
        assert!(
            !screen.contains("src/main.rs"),
            "popup should be gone:\n{screen}"
        );
    }

    #[test]
    fn slash_help_produces_a_local_notice_without_prompting() {
        let mut app = test_app();
        type_text(&mut app, "/help");
        app.on_terminal_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
        app.on_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
        // Handled locally: no run started, a Notice cell lists the commands.
        assert!(!app.is_running());
        let notice = app.transcript.cells.iter().any(|c| {
            matches!(c, Cell::Notice { text } if text.contains("/model") && text.contains("/usage"))
        });
        assert!(notice, "expected /help notice");
    }

    #[test]
    fn slash_quit_quits_and_slash_typing_opens_command_popup() {
        let mut app = test_app();
        type_text(&mut app, "/he");
        assert!(app.autocomplete.is_open(), "command popup should open");
        let screen = draw(&mut app, 80, 20);
        assert!(screen.contains("help"), "popup missing:\n{screen}");
        // Accept via Enter: inserts, does not submit.
        app.on_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
        assert_eq!(app.input.text(), "/help ");

        // Now /quit end-to-end.
        let mut app = test_app();
        type_text(&mut app, "/quit");
        app.on_terminal_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
        app.on_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
        assert!(app.should_quit);
    }

    #[test]
    fn visual_cursor_matches_word_wrapping() {
        // width 10 word-wraps "hello world" as ["hello ", "world"]. A plain
        // column-wrap computation would report (1, 1) here - the regression
        // this test pins down.
        assert_eq!(visual_cursor("hello world", 11, 10), (1, 5));
        // On the chunk boundary (after "hello "): inserting there joins the
        // word "world", so the cursor belongs at the start of line 1.
        assert_eq!(visual_cursor("hello world", 6, 10), (1, 0));
        // Mid-first-chunk stays on line 0.
        assert_eq!(visual_cursor("hello world", 3, 10), (0, 3));
    }

    #[test]
    fn visual_cursor_handles_hard_splits_newlines_and_wide_chars() {
        // A single long word hard-splits by columns: "abcd" / "efgh" / "ij".
        assert_eq!(visual_cursor("abcdefghij", 10, 4), (2, 2));
        // Explicit newlines start fresh visual lines.
        assert_eq!(visual_cursor("ab\nxyz", 6, 4), (1, 3));
        assert_eq!(visual_cursor("ab\n", 3, 4), (1, 0));
        // CJK chars are 2 columns wide: width 4 fits two, the third wraps.
        assert_eq!(visual_cursor("日本語", 3, 4), (1, 2));
        // After the second char = boundary = start of the wrapped chunk.
        assert_eq!(visual_cursor("日本語", 2, 4), (1, 0));
        // Empty buffer.
        assert_eq!(visual_cursor("", 0, 4), (0, 0));
    }

    #[test]
    fn input_viewport_follows_the_cursor_past_the_height_cap() {
        let mut app = test_app();
        // 8 explicit lines; the input box caps at 5 visible rows, so the
        // viewport must scroll to keep the cursor's line (the last one) on
        // screen.
        app.input
            .insert_str("line0\nline1\nline2\nline3\nline4\nline5\nline6\nline7");
        let screen = draw(&mut app, 80, 20);
        assert!(
            screen.contains("line7"),
            "cursor line must be visible:\n{screen}"
        );
        assert!(
            !screen.contains("line0"),
            "scrolled-out line must be hidden:\n{screen}"
        );
    }

    #[test]
    fn resumed_history_replays_into_transcript_cells() {
        use cupel_agent::AgentMessage;
        use cupel_core::types::{
            Api, AssistantContent, AssistantMessage as CoreAssistant, Message, StopReason,
            TextContent, ToolCall, ToolResultMessage, Usage, now_ms,
        };

        // Seed an Agent the way --resume does, then build the App around it.
        let model = cupel_core::catalog::builtin_models().remove(0);
        let registry = Arc::new(cupel_core::provider::Registry::new());
        let assistant = CoreAssistant {
            content: vec![
                AssistantContent::Text(TextContent::plain("the answer")),
                AssistantContent::ToolCall(ToolCall {
                    id: "call_1".into(),
                    name: "grep".into(),
                    arguments: serde_json::json!({"pattern": "bug"}),
                }),
            ],
            api: Api::from("mock"),
            provider: cupel_core::types::Provider::from("mock"),
            model: "mock".into(),
            response_model: None,
            response_id: None,
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: now_ms(),
        };
        let tool_result = ToolResultMessage {
            tool_call_id: "call_1".into(),
            tool_name: "grep".into(),
            content: vec![cupel_core::types::ToolResultContent::Text(
                TextContent::plain("src/main.rs:1: bug"),
            )],
            details: None,
            is_error: false,
            timestamp: now_ms(),
        };
        let mut options = AgentOptions::new(model, registry);
        options.messages = vec![
            AgentMessage::user_text("old question"),
            AgentMessage::Llm(Message::Assistant(assistant)),
            AgentMessage::Llm(Message::ToolResult(tool_result)),
        ];
        let agent = Agent::new(options);
        let recorder = crate::session::SessionRecorder::new(
            None,
            std::path::Path::new("/tmp"),
            "cupel-resumed",
            "test-model",
        );
        let mut app = App::new(
            agent,
            SessionMeta {
                model_name: "Test Model".into(),
                provider: "test".into(),
                cwd: "/tmp".into(),
                templates: Vec::new(),
                // Real builtin catalog so /model and /provider tests exercise
                // the same data the app ships with.
                models: cupel_core::catalog::builtin_models(),
                settings: crate::settings::Settings::default(),
                home: None,
                startup_warning: None,
                context_files: Vec::new(),
            },
            recorder,
        );

        let screen = draw(&mut app, 80, 24);
        assert!(
            screen.contains("resumed session cupel-resumed (3 messages)"),
            "resume notice missing:\n{screen}"
        );
        assert!(screen.contains("old question"), "user cell:\n{screen}");
        assert!(screen.contains("the answer"), "assistant cell:\n{screen}");
        assert!(
            screen.contains("src/main.rs:1: bug"),
            "tool result attached:\n{screen}"
        );
    }

    #[test]
    fn ctrl_y_toggles_selection_mode_and_updates_the_footer() {
        let mut app = test_app();
        assert!(app.mouse_captured);
        let screen = draw(&mut app, 200, 20);
        assert!(screen.contains("ctrl+o copy"), "hint missing:\n{screen}");
        assert!(screen.contains("ctrl+y select"), "hint missing:\n{screen}");

        // Ctrl+Y only REQUESTS the toggle (the event loop owns the
        // terminal); applying flips state and posts a notice.
        app.on_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::Char('y'),
            KeyModifiers::CONTROL,
        )));
        assert!(app.mouse_toggle_requested);
        assert!(!app.apply_mouse_toggle(), "capture now off");
        assert!(!app.mouse_toggle_requested);

        let screen = draw(&mut app, 200, 20);
        assert!(screen.contains("SELECTION MODE"), "hint:\n{screen}");
        assert!(
            screen.contains("selection mode - select and copy"),
            "notice cell missing:\n{screen}"
        );
        // And back on.
        assert!(app.apply_mouse_toggle(), "capture on again");
    }

    #[test]
    fn multi_line_paste_inserts_without_submitting() {
        let mut app = test_app();
        // Bracketed paste delivers the whole clipboard as ONE event; the
        // embedded newline must become buffer content, not an Enter press.
        app.on_terminal_event(Event::Paste("line one\nline two".to_string()));
        assert_eq!(app.input.text(), "line one\nline two");
        assert!(app.pending_prompt.is_none(), "paste must not submit");
        assert!(app.transcript.cells.is_empty());
    }

    #[test]
    fn mouse_wheel_scrolls_the_transcript() {
        use ratatui::crossterm::event::{MouseEvent, MouseEventKind};

        let mut app = test_app();
        for i in 0..50 {
            app.transcript.cells.push(Cell::Assistant {
                text: format!("line {i}"),
            });
        }
        // Render once so the app learns the viewport geometry.
        let _ = draw(&mut app, 40, 10);

        let wheel = |kind| {
            Event::Mouse(MouseEvent {
                kind,
                column: 5,
                row: 5,
                modifiers: KeyModifiers::NONE,
            })
        };
        app.on_terminal_event(wheel(MouseEventKind::ScrollUp));
        assert_eq!(app.scroll_from_bottom, 3, "one notch = three lines");
        app.on_terminal_event(wheel(MouseEventKind::ScrollDown));
        app.on_terminal_event(wheel(MouseEventKind::ScrollDown)); // clamps at 0
        assert_eq!(app.scroll_from_bottom, 0);
    }

    #[test]
    fn model_and_thinking_arguments_autocomplete_end_to_end() {
        let mut app = test_app();
        // Accepting `/model ` from the command popup rolls straight into
        // the model list - no extra keystroke needed.
        type_text(&mut app, "/mod");
        app.on_terminal_event(Event::Key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)));
        assert_eq!(app.input.text(), "/model ");
        assert!(app.autocomplete.is_open(), "model list should open");
        let screen = draw(&mut app, 100, 24);
        assert!(
            screen.contains("claude-sonnet-5  (anthropic)"),
            "catalog rows missing:\n{screen}"
        );

        // Narrow to one model, accept, and the command is ready to submit.
        type_text(&mut app, "haiku");
        app.on_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
        assert_eq!(app.input.text(), "/model claude-haiku-4-5 ");
        assert!(app.pending_prompt.is_none(), "accept must not submit");

        // Same flow for /thinking.
        let mut app = test_app();
        type_text(&mut app, "/thinking of");
        let (rows, selected) = app.autocomplete.visible().expect("levels");
        assert_eq!(rows[selected].value, "off");
        app.on_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
        assert_eq!(app.input.text(), "/thinking off ");
        // The settled argument closed the popup; Enter now submits.
        assert!(!app.autocomplete.is_open());
    }

    #[test]
    fn provider_command_lists_switches_and_takes_a_session_key() {
        let mut app = test_app();

        // `/provider ` opens the provider list via argument autocomplete.
        type_text(&mut app, "/provider ");
        let (rows, _) = app.autocomplete.visible().expect("provider rows");
        let values: Vec<&str> = rows.iter().map(|r| r.value.as_str()).collect();
        assert!(values.contains(&"anthropic"), "{values:?}");
        assert!(values.contains(&"fireworks"), "{values:?}");

        // Bare /provider prints the list with credential status.
        let mut app = test_app();
        type_text(&mut app, "/provider");
        app.on_terminal_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
        app.on_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
        let listing = app.transcript.cells.iter().any(|c| {
            matches!(c, Cell::Notice { text }
                if text.contains("anthropic") && text.contains("amazon-bedrock"))
        });
        assert!(listing, "expected provider listing notice");

        // Switching with an explicit key: session key wins, no echo of the
        // secret, meta + footer follow the new provider's default model.
        let mut app = test_app();
        type_text(&mut app, "/provider fireworks fw-secret-123");
        app.on_terminal_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
        app.on_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
        assert_eq!(app.meta.provider, "fireworks");
        let switched = app.transcript.cells.iter().any(|c| {
            matches!(c, Cell::Notice { text }
                if text.contains("provider switched to fireworks")
                    && text.contains("key entered this session")
                    && !text.contains("fw-secret-123"))
        });
        assert!(switched, "expected switch notice without the secret");

        // Unknown provider: a helpful error, no state change.
        type_text(&mut app, "/provider nope");
        app.on_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
        assert_eq!(app.meta.provider, "fireworks", "unchanged");
        let unknown =
            app.transcript.cells.iter().any(
                |c| matches!(c, Cell::Notice { text } if text.contains("unknown provider: nope")),
            );
        assert!(unknown);
    }

    #[test]
    fn up_down_with_popup_open_move_selection_not_history() {
        let mut app = test_app_in(&autocomplete_cwd("nav"));
        // Prime history directly on the input (App::submit would spawn agent
        // tasks, which needs a tokio runtime this sync test doesn't have).
        app.input.insert_str("old prompt");
        let _ = app.input.submit();
        type_text(&mut app, "@");
        assert!(app.autocomplete.is_open());
        let buffer_before = app.input.text().to_string();
        app.on_terminal_event(Event::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)));
        app.on_terminal_event(Event::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE)));
        assert_eq!(app.input.text(), buffer_before, "history must not fire");
        assert!(app.autocomplete.is_open());
    }

    #[test]
    fn provider_key_is_auto_saved_to_settings() {
        let root = std::env::temp_dir().join("cupel-ui-provider-save");
        let _ = std::fs::remove_dir_all(&root);
        let mut app = test_app_with_home(&root, "cupel-save");

        type_text(&mut app, "/provider fireworks fw-secret-123");
        app.on_terminal_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
        app.on_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));

        // On disk, parsed, and holding the key.
        let path = root.join("home/settings.json");
        let saved = crate::settings::load_settings(&path).unwrap();
        assert_eq!(saved.api_key("fireworks"), Some("fw-secret-123"));
        // The in-memory mirror agrees (the listing reads this, not disk).
        assert_eq!(
            app.meta.settings.api_key("fireworks"),
            Some("fw-secret-123")
        );
        // The notice names the file but never the secret.
        let noticed = app.transcript.cells.iter().any(|c| {
            matches!(c, Cell::Notice { text }
                if text.contains("key saved to") && !text.contains("fw-secret-123"))
        });
        assert!(noticed, "expected a save notice without the secret");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn provider_save_failure_keeps_the_session_key() {
        let root = std::env::temp_dir().join("cupel-ui-provider-save-broken");
        let _ = std::fs::remove_dir_all(&root);
        let mut app = test_app_with_home(&root, "cupel-save-broken");
        let path = root.join("home/settings.json");
        std::fs::write(&path, "{broken").unwrap();

        type_text(&mut app, "/provider fireworks fw-secret-123");
        app.on_terminal_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
        app.on_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));

        // The malformed file was refused, not clobbered...
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{broken");
        // ...the key still works for this session, and the switch happened.
        assert_eq!(
            app.session_keys.get("fireworks").map(String::as_str),
            Some("fw-secret-123")
        );
        assert_eq!(app.meta.provider, "fireworks");
        let warned = app.transcript.cells.iter().any(|c| {
            matches!(c, Cell::Notice { text }
                if text.contains("key not saved") && !text.contains("fw-secret-123"))
        });
        assert!(warned, "expected the not-saved warning");
    }

    /// A key-requiring model on a provider id with no env-var mapping -
    /// the same env-independence trick as main.rs's select_model tests.
    fn acme_model() -> cupel_core::types::Model {
        let mut model = cupel_core::catalog::builtin_models().remove(0);
        model.id = "acme-1".into();
        model.provider = cupel_core::types::Provider::from("acme");
        model.compat = None;
        model
    }

    #[test]
    fn provider_listing_shows_settings_state() {
        let mut app = test_app();
        app.meta.models.push(acme_model());
        app.meta
            .settings
            .providers
            .insert("acme".into(), "k-1".into());

        type_text(&mut app, "/provider");
        app.on_terminal_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
        app.on_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
        let listed = app.transcript.cells.iter().any(|c| {
            matches!(c, Cell::Notice { text }
                if text.contains("acme") && text.contains("key in settings.json"))
        });
        assert!(listed, "expected the settings-backed status line");
    }

    #[test]
    fn custom_provider_now_accepts_a_key() {
        let mut app = test_app();
        app.meta.models.push(acme_model());

        type_text(&mut app, "/provider acme secret-1");
        app.on_terminal_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
        app.on_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
        // Accepted into session memory (the old env_var_name gate is gone);
        // with home: None the save fails soft (NoHome warning), which this
        // test does not mind - the acceptance is the point.
        assert_eq!(
            app.session_keys.get("acme").map(String::as_str),
            Some("secret-1")
        );
        let rejected = app.transcript.cells.iter().any(
            |c| matches!(c, Cell::Notice { text } if text.contains("does not take an API key")),
        );
        assert!(!rejected, "custom providers must accept keys now");

        // Bedrock still refuses one.
        type_text(&mut app, "/provider amazon-bedrock some-key");
        app.on_terminal_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
        app.on_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
        let rejected = app.transcript.cells.iter().any(
            |c| matches!(c, Cell::Notice { text } if text.contains("does not take an API key")),
        );
        assert!(rejected, "bedrock has no key slot");
    }

    #[test]
    fn provider_switch_without_key_never_writes_settings() {
        let root = std::env::temp_dir().join("cupel-ui-provider-nokey");
        let _ = std::fs::remove_dir_all(&root);
        let mut app = test_app_with_home(&root, "cupel-nokey");

        type_text(&mut app, "/provider fireworks");
        app.on_terminal_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
        app.on_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
        assert_eq!(app.meta.provider, "fireworks");
        // Proof that env/no-key paths can never persist anything.
        assert!(!root.join("home/settings.json").exists());
    }

    #[tokio::test]
    async fn hot_reload_picks_up_hand_edited_settings() {
        use crate::modes::interactive::app::ReloadTarget;
        let root = std::env::temp_dir().join("cupel-ui-reload-settings");
        let _ = std::fs::remove_dir_all(&root);
        let mut app = test_app_with_home(&root, "cupel-reload-settings");
        // Point the agent at a custom provider so no exported env var can
        // interfere with the assertion.
        let model = acme_model();
        app.agent.set_model(model.clone());
        app.meta.models.push(model);

        // Simulate the user hand-editing the file while cupel runs.
        std::fs::write(
            root.join("home/settings.json"),
            r#"{"providers": {"acme": "from-disk"}}"#,
        )
        .unwrap();

        let app = app.hot_reload(ReloadTarget::Current).await;
        assert_eq!(app.agent.api_key(), Some("from-disk"));
        assert_eq!(app.meta.settings.api_key("acme"), Some("from-disk"));
    }

    #[tokio::test]
    async fn start_run_leads_with_the_task_and_records_it() {
        let root = std::env::temp_dir().join("cupel-ui-task-cell");
        let _ = std::fs::remove_dir_all(&root);
        let mut app = test_app_with_home(&root, "cupel-task");

        app.start_run("find the bug");
        assert!(
            matches!(app.transcript.cells.first(), Some(Cell::User { text }) if text == "find the bug"),
            "the task must lead the turn"
        );
        let screen = draw(&mut app, 80, 20);
        assert!(screen.contains("> find the bug"), "{screen}");

        // The prompt reached the JSONL transcript too (assistant-only
        // transcripts were the resume-fidelity bug this fixes).
        let path = app
            .recorder
            .sessions_dir()
            .expect("home is set")
            .join("cupel-task.jsonl");
        let (_, messages) = crate::session::load_transcript(&path).unwrap();
        let recorded = matches!(
            messages.first(),
            Some(cupel_agent::AgentMessage::Llm(cupel_core::types::Message::User(user)))
                if matches!(&user.content,
                    cupel_core::types::UserContentBody::Text(t) if t == "find the bug")
        );
        assert!(recorded, "prompt missing from the transcript file");

        // The run against the empty test registry errors in the
        // background; settle it so the test ends cleanly.
        app.agent.abort();
        app.agent.wait_for_idle().await;
    }

    #[tokio::test]
    async fn run_end_promotes_the_final_answer() {
        let mut app = test_app();
        app.on_agent_event(Some(AgentEvent::MessageUpdate {
            event: cupel_core::types::AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: "final words".into(),
            },
        }))
        .await;
        app.on_agent_event(Some(AgentEvent::AgentEnd {
            messages: Vec::new(),
        }))
        .await;
        assert!(
            matches!(app.transcript.cells.last(), Some(Cell::Answer { text }) if text == "final words")
        );
    }

    #[test]
    fn footer_shows_the_thinking_level_for_reasoning_models() {
        let mut app = test_app();
        // AgentOptions::new leaves thinking at None -> "thinking off".
        let screen = draw(&mut app, 120, 20);
        assert!(screen.contains("thinking off"), "{screen}");

        app.agent
            .set_thinking_level(Some(cupel_core::types::ThinkingLevel::Medium));
        let screen = draw(&mut app, 120, 20);
        assert!(screen.contains("thinking medium"), "{screen}");
        assert!(!screen.contains("thinking off"), "{screen}");
    }

    #[test]
    fn tools_pane_appears_with_the_first_tool_call() {
        let mut app = test_app();
        app.transcript.cells.push(Cell::Assistant {
            text: "chatting".into(),
        });
        let screen = draw(&mut app, 80, 20);
        assert!(screen.contains(" conversation "), "{screen}");
        assert!(
            !screen.contains(" tools "),
            "no tool traffic yet:\n{screen}"
        );

        app.transcript.cells.push(Cell::Tool {
            id: "1".into(),
            name: "grep".into(),
            args: "{}".into(),
            result: None,
        });
        let screen = draw(&mut app, 80, 20);
        assert!(screen.contains(" tools "), "pane must appear:\n{screen}");
        assert!(screen.contains("[grep]"), "{screen}");
    }

    #[test]
    fn band_rule_ties_reasoning_and_tool_rows_together() {
        let mut app = test_app();
        app.transcript.cells.push(Cell::User {
            text: "task".into(),
        });
        app.transcript.append_thinking("let me look");
        app.transcript.cells.push(Cell::Tool {
            id: "1".into(),
            name: "read".into(),
            args: "{}".into(),
            result: None,
        });
        let screen = draw(&mut app, 90, 20);
        // The SAME band number must sit on ONE screen row in both panes -
        // that shared row is the reasoning->tool association.
        let banded = screen.lines().any(|row| row.matches("─ 1 ").count() == 2);
        assert!(banded, "band number missing or misaligned:\n{screen}");
    }

    #[test]
    fn scrollbar_appears_once_content_overflows() {
        let mut app = test_app();
        let screen = draw(&mut app, 40, 12);
        assert!(
            !screen.contains('█'),
            "an empty transcript needs no scrollbar:\n{screen}"
        );
        for i in 0..40 {
            app.transcript.cells.push(Cell::Assistant {
                text: format!("line {i}"),
            });
        }
        let screen = draw(&mut app, 40, 12);
        assert!(
            screen.contains('█'),
            "overflowing content must show the thumb:\n{screen}"
        );
    }

    #[test]
    fn click_selects_a_block_and_ctrl_o_copies_it() {
        use ratatui::crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

        let mut app = test_app();
        app.transcript.cells.push(Cell::User {
            text: "the task".into(),
        });
        app.transcript.append_thinking("private reasoning");
        app.transcript.append_assistant("the answer");
        let _ = draw(&mut app, 80, 24); // teach the app its geometry

        // Resolve the thinking cell's screen row through the same map the
        // click handler uses - the test then exercises the real geometry
        // math instead of hardcoding a row.
        let line = app
            .last_line_cells
            .iter()
            .position(|cell| *cell == Some(1))
            .expect("thinking line mapped");
        let row = app.last_chat_inner.y + (line - app.last_top_line) as u16;
        app.on_terminal_event(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: app.last_chat_inner.x + 1,
            row,
            modifiers: KeyModifiers::NONE,
        }));
        assert_eq!(app.selected_cell, Some(1));

        // The selected block renders with the highlight background.
        let _ = draw(&mut app, 80, 24);
        let style = style_of(&mut app, "private reasoning");
        assert_eq!(style.bg, theme::SELECTED.bg);

        // Ctrl+O queues the RAW text and confirms with a notice.
        app.on_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::Char('o'),
            KeyModifiers::CONTROL,
        )));
        assert_eq!(app.pending_copy.as_deref(), Some("private reasoning"));
        assert!(
            app.transcript
                .cells
                .iter()
                .any(|c| matches!(c, Cell::Notice { text } if text.contains("clipboard")))
        );

        // Esc drops the selection.
        app.on_terminal_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
        assert_eq!(app.selected_cell, None);
    }

    #[test]
    fn ctrl_o_with_nothing_selected_copies_the_latest_answer() {
        let mut app = test_app();
        app.transcript.cells.push(Cell::Answer {
            text: "first".into(),
        });
        app.transcript.cells.push(Cell::Answer {
            text: "final answer".into(),
        });
        app.on_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::Char('o'),
            KeyModifiers::CONTROL,
        )));
        assert_eq!(app.pending_copy.as_deref(), Some("final answer"));

        // An empty transcript: a helpful notice, nothing queued.
        let mut app = test_app();
        app.on_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::Char('o'),
            KeyModifiers::CONTROL,
        )));
        assert!(app.pending_copy.is_none());
        assert!(
            app.transcript
                .cells
                .iter()
                .any(|c| matches!(c, Cell::Notice { text } if text.contains("nothing to copy")))
        );
    }

    #[test]
    fn markdown_reaches_the_screen_and_plain_answers_stay_magenta() {
        let mut app = test_app();
        app.transcript.cells.push(Cell::Answer {
            text: "# Fazit\nplain magenta with **weight**".into(),
        });
        let screen = draw(&mut app, 80, 24);
        assert!(screen.contains("Fazit"), "{screen}");
        assert!(!screen.contains("# Fazit"), "hashes are chrome: {screen}");
        assert!(!screen.contains("**"), "delimiters are consumed: {screen}");
        // The invariant: unmarked text keeps the cell identity...
        assert_eq!(style_of(&mut app, "plain magenta").fg, Some(Color::Magenta));
        // ...and accents COMBINE with it instead of replacing it.
        let weight = style_of(&mut app, "weight");
        assert_eq!(weight.fg, Some(Color::Magenta));
        assert!(weight.add_modifier.contains(Modifier::BOLD));
    }
}
