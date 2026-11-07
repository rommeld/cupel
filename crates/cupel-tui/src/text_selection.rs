//! Selection coordinates refer to rendered transcript rows, not viewport rows.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Line;
use unicode_width::UnicodeWidthStr as _;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Point {
    pub row: usize,
    pub column: usize,
}

pub struct TextSelection {
    pub anchor: Point,
    pub end: Point,
    pub dragging: bool,
    pub pointer_row: usize,
}

impl TextSelection {
    pub fn new(point: Point, pointer_row: usize) -> Self {
        Self {
            anchor: point,
            end: point,
            dragging: true,
            pointer_row,
        }
    }

    pub fn is_range(&self) -> bool {
        self.anchor != self.end
    }

    fn bounds(&self) -> (Point, Point) {
        (self.anchor.min(self.end), self.anchor.max(self.end))
    }

    fn columns(&self, row: usize) -> Option<std::ops::Range<usize>> {
        let (start, end) = self.bounds();
        (start.row..=end.row).contains(&row).then(|| {
            let left = if row == start.row { start.column } else { 0 };
            let right = if row == end.row {
                end.column.saturating_add(1)
            } else {
                usize::MAX
            };
            left..right
        })
    }

    /// Ignore style changes, but invalidate coordinates when preceding text changes.
    pub fn matches(&self, old: &[Line<'_>], new: &[Line<'_>]) -> bool {
        let (_, end) = self.bounds();
        old.len() > end.row
            && new.len() > end.row
            && old[..=end.row].iter().zip(&new[..=end.row]).all(|(a, b)| {
                a.spans
                    .iter()
                    .flat_map(|span| span.content.chars())
                    .eq(b.spans.iter().flat_map(|span| span.content.chars()))
            })
    }

    /// Copy visible text, including complete wide/combining graphemes.
    pub fn text(&self, lines: &[Line<'_>]) -> String {
        let (start, end) = self.bounds();
        let mut rows = Vec::new();
        for (row, line) in lines.iter().enumerate().take(end.row + 1).skip(start.row) {
            let Some(columns) = self.columns(row) else {
                continue;
            };
            let mut text = String::new();
            let mut column = 0;
            for grapheme in line.styled_graphemes(Style::default()) {
                let next = column + grapheme.symbol.width();
                if column < columns.end && next > columns.start {
                    text.push_str(grapheme.symbol);
                }
                column = next;
            }
            rows.push(text.trim_end().to_string());
        }
        rows.join("\n")
    }

    pub fn highlight(&self, buffer: &mut Buffer, inner: Rect, top: usize, style: Style) {
        if !self.is_range() {
            return;
        }
        for y in inner.y..inner.bottom() {
            let Some(columns) = self.columns(top + usize::from(y - inner.y)) else {
                continue;
            };
            let mut column = 0;
            while column < usize::from(inner.width) {
                let x = inner.x + column as u16;
                let width = buffer[(x, y)].symbol().width().max(1);
                if column < columns.end && column + width > columns.start {
                    for offset in 0..width.min(usize::from(inner.width) - column) {
                        buffer[(x + offset as u16, y)].set_style(style);
                    }
                }
                column += width;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Point, TextSelection};
    use crate::app::App;
    use crate::sessions::{Session, Sessions};
    use crate::theme::CUPEL;
    use crate::transcript::Cell;
    use crate::ui::tests::test_app;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::crossterm::event::{
        Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use ratatui::layout::Rect;
    use ratatui::style::{Color, Modifier, Style};
    use ratatui::text::Line;
    use ratatui::widgets::Paragraph;

    fn draw(app: &mut App, width: u16) -> Buffer {
        let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
        terminal
            .draw(|frame| crate::ui::render(frame, app, frame.area(), &CUPEL))
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn mouse(app: &mut App, kind: MouseEventKind, column: u16, row: u16) {
        app.on_terminal_event(Event::Mouse(MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }));
    }

    fn key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
        app.on_terminal_event(Event::Key(KeyEvent::new(code, modifiers)));
    }

    fn short_selection() -> App {
        let mut app = test_app();
        app.transcript.append_assistant("alpha bravo charlie");
        draw(&mut app, 80);
        let inner = app.last_transcript_inner;
        mouse(
            &mut app,
            MouseEventKind::Down(MouseButton::Left),
            inner.x + 6,
            inner.y,
        );
        mouse(
            &mut app,
            MouseEventKind::Drag(MouseButton::Left),
            inner.x + 10,
            inner.y,
        );
        app
    }

    #[test]
    fn selection_copies_whole_graphemes_in_either_direction() {
        let lines = vec![Line::from("a日e\u{301}👩‍💻z  "), Line::from("second")];
        let mut selection = TextSelection::new(Point { row: 0, column: 2 }, 0);
        selection.end = Point { row: 0, column: 4 };
        assert_eq!(selection.text(&lines), "日e\u{301}👩‍💻");
        core::mem::swap(&mut selection.anchor, &mut selection.end);
        assert_eq!(selection.text(&lines), "日e\u{301}👩‍💻");
        selection.anchor = Point { row: 0, column: 3 };
        selection.end = Point { row: 1, column: 2 };
        assert_eq!(selection.text(&lines), "e\u{301}👩‍💻z\nsec");
    }

    #[test]
    fn highlight_matches_wide_grapheme_copy_and_preserves_foreground() {
        use ratatui::widgets::Widget as _;

        let mut selection = TextSelection::new(Point { row: 0, column: 2 }, 0);
        selection.end = Point { row: 0, column: 3 };
        let line = Line::styled(
            "a日bc",
            Style::new().fg(Color::Red).add_modifier(Modifier::BOLD),
        );
        let area = Rect::new(0, 0, 8, 1);
        let mut buffer = Buffer::empty(area);
        Paragraph::new(line).render(area, &mut buffer);
        selection.highlight(&mut buffer, area, 0, Style::new().bg(Color::Blue));
        for x in 1..=3 {
            assert_eq!(buffer[(x, 0)].bg, Color::Blue);
        }
        for x in [0, 4] {
            assert_ne!(buffer[(x, 0)].bg, Color::Blue);
        }
        assert_eq!(buffer[(1, 0)].fg, Color::Red);
        assert!(buffer[(1, 0)].modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn drag_selects_only_the_range_and_release_stops_extending() {
        let mut app = short_selection();
        let inner = app.last_transcript_inner;
        let buffer = draw(&mut app, 80);
        assert!(app.mouse_captured);
        assert_eq!(app.selected_cell, None);
        assert_eq!(
            buffer[(inner.x + 6, inner.y)].bg,
            CUPEL.selected.bg.unwrap()
        );
        assert_ne!(
            buffer[(inner.x + 5, inner.y)].bg,
            CUPEL.selected.bg.unwrap()
        );
        mouse(
            &mut app,
            MouseEventKind::Up(MouseButton::Left),
            inner.x + 10,
            inner.y,
        );
        mouse(
            &mut app,
            MouseEventKind::Drag(MouseButton::Left),
            inner.x + 18,
            inner.y,
        );
        key(&mut app, KeyCode::Char('o'), KeyModifiers::CONTROL);
        assert_eq!(app.pending_copy.as_deref(), Some("bravo"));
        key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(app.text_selection.is_none());
    }

    #[test]
    fn wheel_extends_a_drag_across_viewports_and_preserves_a_released_selection() {
        let mut app = test_app();
        app.transcript.cells.push(Cell::Assistant {
            text: (0..60)
                .map(|i| format!("line {i:02}"))
                .collect::<Vec<_>>()
                .join("\n"),
        });
        draw(&mut app, 80);
        key(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
        draw(&mut app, 80);
        let inner = app.last_transcript_inner;
        let start = app.last_top_line;
        mouse(
            &mut app,
            MouseEventKind::Down(MouseButton::Left),
            inner.x,
            inner.y,
        );
        for _ in 0..3 {
            mouse(&mut app, MouseEventKind::ScrollDown, inner.x + 6, inner.y);
            draw(&mut app, 80);
        }
        let end = app.last_top_line;
        mouse(
            &mut app,
            MouseEventKind::Up(MouseButton::Left),
            inner.x + 6,
            inner.y,
        );
        draw(&mut app, 80);
        assert!(app.mouse_captured);
        assert!(end > start);
        mouse(&mut app, MouseEventKind::ScrollUp, inner.x, inner.y);
        draw(&mut app, 80);
        key(&mut app, KeyCode::Char('o'), KeyModifiers::CONTROL);
        let expected = (start..=end)
            .map(|i| format!("line {i:02}"))
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(app.pending_copy.as_deref(), Some(expected.as_str()));
        draw(&mut app, 80);
        assert!(
            app.text_selection.is_some(),
            "copy notice must not invalidate the selection"
        );
    }

    #[test]
    fn selections_survive_appends_but_not_reflow_or_changes_before_the_range() {
        let mut app = short_selection();
        app.transcript.cells.push(Cell::Notice {
            text: "new output".into(),
        });
        draw(&mut app, 80);
        assert!(app.text_selection.is_some());
        draw(&mut app, 60);
        assert!(app.text_selection.is_none());

        let mut app = short_selection();
        app.transcript.cells.insert(
            0,
            Cell::Notice {
                text: "inserted".into(),
            },
        );
        draw(&mut app, 80);
        assert!(app.text_selection.is_none());
    }

    #[test]
    fn focus_resize_toggle_and_session_switch_do_not_leave_a_live_drag() {
        for event in [Event::FocusLost, Event::Resize(60, 24)] {
            let mut app = short_selection();
            app.on_terminal_event(event);
            assert!(
                !app.text_selection
                    .as_ref()
                    .is_some_and(|selection| selection.dragging)
            );
        }
        let mut app = short_selection();
        app.apply_mouse_toggle();
        assert!(app.text_selection.is_none());

        let mut sessions = Sessions::new(short_selection());
        sessions.list.push(Session {
            label: "second".into(),
            app: test_app(),
        });
        for code in ['n', 'p'] {
            sessions.on_terminal_event(Event::Key(KeyEvent::new(
                KeyCode::Char(code),
                KeyModifiers::CONTROL,
            )));
        }
        assert!(!sessions.active().text_selection.as_ref().unwrap().dragging);
    }
}
