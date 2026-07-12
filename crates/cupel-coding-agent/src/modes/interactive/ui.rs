//! The render pass: `App` state in, one frame out.
//!
//! ratatui is immediate mode - this function redescribes the ENTIRE screen
//! every frame, and the library diffs against the previous frame to emit
//! minimal terminal writes. So there is no "update the widget" anywhere;
//! there is only state (in `App`) and this projection of it.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use super::app::App;
use super::transcript;

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
                Style::new().add_modifier(Modifier::REVERSED)
            } else {
                Style::new().fg(Color::Cyan)
            };
            Line::from(Span::styled(format!(" {} ", row.display), style))
        })
        .collect();
    frame.render_widget(Paragraph::new(lines), popup);
}

fn render_transcript(frame: &mut Frame<'_>, app: &mut App, area: Rect) {
    let lines = app.transcript.to_lines(area.width);
    let height = area.height as usize;

    // Remember geometry so key handlers can clamp scrolling next event.
    app.last_total_lines = lines.len();
    app.last_transcript_height = area.height;
    let max_scroll = lines.len().saturating_sub(height);
    app.scroll_from_bottom = app.scroll_from_bottom.min(max_scroll);

    // Bottom-anchored window: offset 0 shows the newest lines.
    let end = lines.len() - app.scroll_from_bottom;
    let start = end.saturating_sub(height);
    let visible: Vec<Line<'static>> = lines[start..end].to_vec();

    frame.render_widget(Paragraph::new(visible), area);

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
            Paragraph::new(marker).style(Style::new().fg(Color::Black).bg(Color::Yellow)),
            marker_area,
        );
    }
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
        Style::new().fg(Color::Yellow)
    } else {
        Style::new().fg(Color::DarkGray)
    };
    let title = if app.is_running() {
        " working - enter queues a steering message "
    } else {
        " prompt "
    };
    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(Span::styled(
            title,
            Style::new().add_modifier(Modifier::DIM),
        ));
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
    let left = format!(
        " {} ({}) | {} | {} in / {} out / {} cached | ${:.4}",
        app.meta.model_name,
        app.meta.provider,
        state,
        app.totals.input,
        app.totals.output,
        app.totals.cache_read,
        app.totals.cost,
    );
    // The mouse hint tracks selection mode, so it never lies about what
    // the wheel currently does.
    let right = if app.mouse_captured {
        "enter send · alt+enter newline · @ file · / cmds · esc abort · wheel scroll · ctrl+y copy "
    } else {
        "enter send · alt+enter newline · @ file · / cmds · esc abort · SELECTION MODE · ctrl+y scroll "
    };

    // Left-align the status, right-align the key hints; drop the hints when
    // the terminal is too narrow for both.
    let mut spans = vec![Span::styled(
        left.clone(),
        Style::new().add_modifier(Modifier::DIM),
    )];
    let padding = (area.width as usize).saturating_sub(left.len() + right.len());
    if padding > 0 {
        spans.push(Span::raw(" ".repeat(padding)));
        spans.push(Span::styled(
            right,
            Style::new().add_modifier(Modifier::DIM),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

#[cfg(test)]
mod tests {
    //! Headless render tests: ratatui's `TestBackend` draws frames into an
    //! in-memory buffer, so the full render path is testable without a
    //! terminal (or an API key - the Agent is constructed but never run).

    use super::*;
    use crate::modes::SessionMeta;
    use crate::modes::interactive::transcript::{Cell, ToolOutcome};
    use cupel_agent::{Agent, AgentOptions};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
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
                    thought_signature: None,
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
        assert!(screen.contains("ctrl+y copy"), "hint missing:\n{screen}");

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
            screen.contains("claude-sonnet-4-5  (anthropic)"),
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
}
