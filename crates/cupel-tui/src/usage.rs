//! A modal, scrollable usage table. Disk scanning runs off the UI thread.

use std::path::PathBuf;

use cupel_coding_agent::usage::{HistoryUsage, UsageByModel, UsageTotals};
use ratatui::Frame;
use ratatui::crossterm::event::{Event, KeyCode, KeyEventKind, MouseButton, MouseEventKind};
use ratatui::layout::{Constraint, Position, Rect};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table};
use tokio::task::{JoinError, JoinHandle};

use crate::terminal_text::sanitize;
use crate::theme::Theme;

pub enum HistoryState {
    Loading(JoinHandle<HistoryUsage>),
    Ready(HistoryUsage),
    Failed(String),
}

pub struct UsagePopup {
    pub current: UsageByModel,
    pub history: HistoryState,
    pub scroll: usize,
    pub content_rows: usize,
    pub viewport_rows: usize,
    pub close_button: Rect,
}

impl UsagePopup {
    #[must_use]
    pub fn new(current: UsageByModel, home: Option<PathBuf>, exclude: Option<PathBuf>) -> Self {
        let task = tokio::task::spawn_blocking(move || {
            cupel_coding_agent::usage::load_history(home.as_deref(), exclude.as_deref())
        });
        Self {
            current,
            history: HistoryState::Loading(task),
            scroll: 0,
            content_rows: 0,
            viewport_rows: 0,
            close_button: Rect::ZERO,
        }
    }

    pub async fn next_event(&mut self) -> Result<HistoryUsage, JoinError> {
        match &mut self.history {
            HistoryState::Loading(task) => task.await,
            HistoryState::Ready(_) | HistoryState::Failed(_) => std::future::pending().await,
        }
    }

    pub fn finish_loading(&mut self, result: Result<HistoryUsage, JoinError>) {
        self.history = match result {
            Ok(history) => HistoryState::Ready(history),
            Err(error) => HistoryState::Failed(format!("Cannot load historical usage: {error}")),
        };
    }

    /// All input is modal; true asks the owner to close the popup.
    pub fn on_terminal_event(&mut self, event: &Event) -> bool {
        match event {
            Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::Esc => return true,
                KeyCode::Up => self.scroll = self.scroll.saturating_sub(1),
                KeyCode::Down => self.scroll = self.scroll.saturating_add(1),
                KeyCode::PageUp => {
                    self.scroll = self.scroll.saturating_sub(self.viewport_rows.max(1));
                }
                KeyCode::PageDown => {
                    self.scroll = self.scroll.saturating_add(self.viewport_rows.max(1));
                }
                KeyCode::Home => self.scroll = 0,
                KeyCode::End => self.scroll = self.content_rows,
                _ => {}
            },
            Event::Mouse(mouse) => match mouse.kind {
                MouseEventKind::Down(MouseButton::Left)
                    if self
                        .close_button
                        .contains(Position::new(mouse.column, mouse.row)) =>
                {
                    return true;
                }
                MouseEventKind::ScrollUp => self.scroll = self.scroll.saturating_sub(3),
                MouseEventKind::ScrollDown => self.scroll = self.scroll.saturating_add(3),
                _ => {}
            },
            _ => {}
        }
        self.clamp_scroll();
        false
    }

    fn clamp_scroll(&mut self) {
        self.scroll = self
            .scroll
            .min(self.content_rows.saturating_sub(self.viewport_rows));
    }
}

/// Painted after the normal screen. The title's X is also the hit-test rectangle.
pub fn render(frame: &mut Frame<'_>, popup: &mut UsagePopup, area: Rect, theme: &Theme) {
    popup.close_button = Rect::ZERO;
    if area.width < 3 || area.height < 3 {
        return;
    }
    let width = area.width.saturating_sub(4).clamp(3, 150);
    let height = area.height.saturating_sub(2).clamp(3, 40);
    let area = Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    );
    let block = Block::new()
        .borders(Borders::ALL)
        .style(theme.popup_row)
        .border_style(theme.pane_border)
        .title(Span::styled("[X] Usage", theme.chrome))
        .title_bottom(Span::styled(
            " Esc close | arrows/PgUp/PgDn scroll",
            theme.chrome,
        ));
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    // Ratatui starts a left title immediately after the top-left corner.
    popup.close_button = Rect::new(area.x + 1, area.y, 3.min(inner.width), 1);
    if inner.width < 56 || inner.height < 5 {
        frame.render_widget(
            Paragraph::new("Enlarge terminal to view usage; Esc closes."),
            inner,
        );
        return;
    }

    let mut rows = Vec::new();
    let mut widths = [2_u16, 3, 6, 4];
    append_section(
        &mut rows,
        &mut widths,
        "CURRENT SESSION",
        &popup.current,
        theme,
    );
    let status = match &popup.history {
        HistoryState::Loading(_) => {
            rows.push(Row::new(vec!["ALL SESSIONS - loading..."]));
            "Loading history in the background".to_string()
        }
        HistoryState::Failed(error) => {
            rows.push(Row::new(vec!["ALL SESSIONS - unavailable"]));
            error.clone()
        }
        HistoryState::Ready(history) => {
            let mut all = history.usage.clone();
            all.merge(&popup.current);
            append_section(&mut rows, &mut widths, "ALL SESSIONS", &all, theme);
            history.warnings.first().map_or_else(
                || format!("{} other transcripts + current snapshot", history.sessions),
                |warning| {
                    format!(
                        "Partial history ({} warnings): {warning}",
                        history.warnings.len()
                    )
                },
            )
        }
    };
    let table_area = Rect {
        height: inner.height.saturating_sub(2),
        ..inner
    };
    popup.content_rows = rows.len();
    popup.viewport_rows = usize::from(table_area.height.saturating_sub(1));
    popup.clamp_scroll();
    let numeric_width: u16 = widths.iter().sum();
    if numeric_width + 4 + 12 > inner.width {
        frame.render_widget(
            Paragraph::new("Enlarge terminal to show full token counts."),
            table_area,
        );
    } else {
        let table = Table::new(
            rows.into_iter().skip(popup.scroll),
            [
                Constraint::Min(12),
                Constraint::Length(widths[0]),
                Constraint::Length(widths[1]),
                Constraint::Length(widths[2]),
                Constraint::Length(widths[3]),
            ],
        )
        .column_spacing(1)
        .header(Row::new(["Provider / model", "In", "Out", "Cached", "Cost"]).style(theme.chrome));
        frame.render_widget(table, table_area);
    }
    let notes = Rect {
        y: table_area.bottom(),
        height: 2,
        ..inner
    };
    frame.render_widget(
        Paragraph::new(format!(
            "{}\nSnapshot; cached = reads; output includes reasoning; rows {}-{}/{}",
            sanitize(&status).replace('\n', " "),
            popup.scroll + 1,
            (popup.scroll + popup.viewport_rows).min(popup.content_rows),
            popup.content_rows,
        )),
        notes,
    );
}

fn append_section(
    rows: &mut Vec<Row<'static>>,
    widths: &mut [u16; 4],
    title: &'static str,
    usage: &UsageByModel,
    theme: &Theme,
) {
    rows.push(Row::new(vec![title]).style(theme.chrome));
    if usage.models.is_empty() {
        rows.push(Row::new(vec!["(no reported usage)"]));
    }
    for (key, totals) in &usage.models {
        rows.push(usage_row(
            sanitize(&format!("{}/{}", key.provider, key.model)).replace('\n', " "),
            *totals,
            widths,
        ));
    }
    rows.push(usage_row("TOTAL".to_string(), usage.totals(), widths).style(theme.chrome));
    rows.push(Row::new(vec![""]));
}

fn usage_row(label: String, totals: UsageTotals, widths: &mut [u16; 4]) -> Row<'static> {
    let numbers = [
        totals.input.to_string(),
        totals.output.to_string(),
        totals.cache_read.to_string(),
        format!("${:.4}", totals.cost),
    ];
    for (width, text) in widths.iter_mut().zip(&numbers) {
        *width = (*width).max(u16::try_from(text.len()).unwrap_or(u16::MAX));
    }
    Row::new(
        std::iter::once(Cell::from(label)).chain(
            numbers
                .into_iter()
                .map(|text| Cell::from(Line::from(text).right_aligned())),
        ),
    )
}
