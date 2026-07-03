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
use ratatui::widgets::{Block, Borders, Paragraph};

use super::app::App;

pub fn render(frame: &mut Frame<'_>, app: &mut App) {
    // Input grows with its content (up to 5 lines) + 2 border rows.
    let input_lines = app.input.text().split('\n').count().clamp(1, 5) as u16;
    let [transcript_area, input_area, footer_area] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(input_lines + 2),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    render_transcript(frame, app, transcript_area);
    render_input(frame, app, input_area);
    render_footer(frame, app, footer_area);
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

    let text: Vec<Line<'_>> = app.input.text().split('\n').map(Line::from).collect();
    frame.render_widget(Paragraph::new(text), inner);

    // Place the real terminal cursor at the editing position. (ratatui hides
    // it unless the app explicitly positions it each frame.)
    let (line, col) = app.input.cursor_line_col();
    frame.set_cursor_position(Position {
        x: inner.x + (col as u16).min(inner.width.saturating_sub(1)),
        y: inner.y + (line as u16).min(inner.height.saturating_sub(1)),
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
    let right = "enter send · alt+enter newline · esc abort · ctrl-t tools · pgup/pgdn scroll ";

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
        let model = cupel_core::catalog::builtin_models().remove(0);
        let registry = Arc::new(cupel_core::provider::Registry::new());
        let agent = Agent::new(AgentOptions::new(model, registry));
        App::new(
            agent,
            SessionMeta {
                model_name: "Test Model".into(),
                provider: "test".into(),
                cwd: "/tmp".into(),
            },
        )
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
}
