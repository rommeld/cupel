//! The render pass: `App` state in, one frame out.
//!
//! ratatui is immediate mode this function redescribes the entire screen every frame,
//! and the library diffs against the previous frame to emit minimal terminal writes.
//! So there is no "update the widget" anywhere; there is only state (in `App`) and
//! this projection of it.

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Clear, Padding, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState,
};

use crate::app::{App, Status, format_elapsed};
use crate::sessions::Sessions;
use crate::terminal_text::sanitize;
use crate::{theme::Theme, transcript};

const SIDEBAR_WIDTH: u16 = 24;
const SIDEBAR_MIN_SCREEN: u16 = 80;
const INPUT_PADDING: u16 = 1;
const QUESTION_PADDING: u16 = 2;

pub fn render_sessions(frame: &mut Frame<'_>, sessions: &mut Sessions) {
    // A second owner leaves `sessions` free for mutable borrows below.
    // Arc::clone copies a pointer and counts, not the theme.
    let theme = std::sync::Arc::clone(&sessions.theme);
    let area = frame.area();
    // Start every cell in the theme's colors; later widgets patch them.
    // CUPEL's empty base leaves the terminal colors unchanged.
    frame.render_widget(Block::new().style(theme.base), area);
    let width = if sessions.list.len() > 1 && area.width >= SIDEBAR_MIN_SCREEN {
        SIDEBAR_WIDTH
    } else {
        0
    };
    let [sidebar, main] =
        Layout::horizontal([Constraint::Length(width), Constraint::Min(1)]).areas(area);
    render_sidebar(frame, sessions, sidebar, &theme);
    render(frame, &mut sessions.list[sessions.active].app, main, &theme);
}

/// A mark for each session's state, its label, and the session on screen highlighted.
/// It revords where each row was drawn, so a click can find its session.
fn render_sidebar(frame: &mut Frame<'_>, sessions: &mut Sessions, area: Rect, theme: &Theme) {
    sessions.rows.clear();
    if area.width == 0 {
        return;
    }
    let block = pane_block(Some("sessions"), theme)
        .title_bottom(Span::styled(" ctrl+n/p · click ", theme.chrome));
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let mut lines = Vec::new();
    for (index, session) in sessions.list.iter().enumerate() {
        let mark = match session.app.status() {
            Status::Running => SPINNER[session.app.frame % SPINNER.len()],
            Status::Asking => "?",
            Status::Done => "✓",
            Status::Failed => "✗",
            Status::New | Status::Cancelled => "○",
        };
        let count = sessions.conflict_count(index);
        let badge = if count == 0 {
            String::new()
        } else {
            format!(" !{count}")
        };
        let room = usize::from(inner.width).saturating_sub(2 + badge.len());
        let text = format!("{mark} {:<room$}", fit(&session.label, room));
        let style = if index == sessions.active {
            theme.popup_selected
        } else {
            ratatui::style::Style::default()
        };
        lines.push(Line::from(vec![
            Span::styled(text, style),
            Span::styled(badge, style.patch(theme.error)),
        ]));
        let y = inner.y + index as u16;
        if y < inner.bottom() {
            let row = Rect {
                x: area.x,
                y,
                width: area.width,
                height: 1,
            };
            sessions.rows.push((row, index));
        }
    }
    frame.render_widget(Paragraph::new(lines), inner);
}

fn fit(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut cut: String = text.chars().take(max.saturating_sub(1)).collect();
    cut.push('…');
    cut
}

pub fn render(frame: &mut Frame<'_>, app: &mut App, area: Rect, theme: &Theme) {
    // Input grows with its content (explicit newlines + wrapped lines), capped at 5
    // visible lines, + 2 border rows. Account for the same horizontal padding used by
    // the editor so wrapping and height agree.
    let inner_width = area.width.saturating_sub(2 * INPUT_PADDING).max(1) as usize;
    let input_lines = app
        .input
        .text()
        .split('\n')
        .map(|line| transcript::wrap_line(line, inner_width).len())
        .sum::<usize>()
        .clamp(1, 5) as u16;
    let waiting = app.steering.len() + app.queued.len() + usize::from(app.waiting_helper.is_some());
    let queued_lines = if waiting == 0 { 0 } else { waiting as u16 + 1 };
    // Reserve the editor, model status, footer, queues and one transcript row first.
    // The pane itself uses at most half the screen; its outside gaps are separate rows.
    let available = area.height.saturating_sub(
        queued_lines
            .saturating_add(input_lines + 2)
            .saturating_add(4),
    );
    let question_gap = u16::from(app.question.is_some() && available >= 9);
    let question_budget = (area.height / 2).min(available.saturating_sub(2 * question_gap));
    let question = question_pane(
        app,
        Rect {
            height: question_budget,
            ..area
        },
        theme,
    );
    let question_rows = question.as_ref().map_or(0, |pane| pane.height);
    let [
        transcript_area,
        _,
        question_area,
        _,
        queued_area,
        model_area,
        input_area,
        footer_area,
    ] = Layout::vertical([
        Constraint::Min(1),
        Constraint::Length(question_gap),
        Constraint::Length(question_rows),
        Constraint::Length(question_gap),
        Constraint::Length(queued_lines),
        Constraint::Length(1),
        Constraint::Length(input_lines + 2),
        Constraint::Length(2),
    ])
    .areas(area);

    render_transcript(frame, app, transcript_area, theme);
    if let Some(question) = question {
        render_question(frame, question, question_area, theme);
    }
    render_queued(frame, app, queued_area, theme);
    render_model_status(frame, app, model_area, theme);
    render_input(frame, app, input_area, theme);
    render_footer(frame, app, footer_area, theme);
    // Drawn last so it overdraws the transcript's bottom rows in
    // immediate-mode rendering, paint order is the z-order.
    render_autocomplete(frame, app, area, transcript_area, model_area, theme);
    if let Some(popup) = &mut app.usage_popup {
        crate::usage::render(frame, popup, area, theme);
    }
}

/// The `@path` completion popup, anchored above the model status line at the
/// column of the token's `@`, leaving the model and effort visible while typing.
fn render_autocomplete(
    frame: &mut Frame<'_>,
    app: &App,
    area: Rect,
    transcript_area: Rect,
    anchor_area: Rect,
    theme: &Theme,
) {
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
        .min(area.width);
    let x = (anchor_area.x + INPUT_PADDING + anchor_col).min(area.right().saturating_sub(width));

    let popup = Rect {
        x,
        y: anchor_area.y.saturating_sub(height),
        width,
        height,
    };

    // Clear blanks the transcript underneath, then the rows paint on top.
    // It also resets colors: the paragraph repaints the entire popup surface,
    // including padding after shorter rows.
    frame.render_widget(Clear, popup);
    let lines: Vec<Line<'_>> = rows
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let style = if i == selected {
                theme.popup_selected
            } else {
                theme.popup_row
            };
            Line::from(Span::styled(format!(" {} ", row.display), style))
        })
        .collect();
    frame.render_widget(Paragraph::new(lines).style(theme.popup_row), popup);
}

fn render_transcript(frame: &mut Frame<'_>, app: &mut App, area: Rect, theme: &Theme) {
    // Leave a small margin on every side without drawing borders.
    let block = pane_block(None, theme)
        .borders(Borders::NONE)
        .padding(Padding::uniform(1));
    let inner = block.inner(area);
    frame.render_widget(block, area);

    let rendered = app
        .transcript
        .to_lines(inner.width, app.selected_cell, theme);
    let total = rendered.lines.len();
    let height = inner.height as usize;

    // Keep the reader's place: the offset is measured from the bottom, and
    // new output moves the bottom a fixed offset would slide the view.
    // Growing it by exactly the growth pins the visible lines; at 0
    // (follow mode) the view sticks to the tail on purpose.
    if app.scroll_from_bottom > 0 {
        app.scroll_from_bottom += total.saturating_sub(app.last_total_lines);
    }

    // Remember geometry so key/mouse handlers can clamp and hit-test.
    app.last_total_lines = total;
    app.last_transcript_height = inner.height;
    let max_scroll = total.saturating_sub(height);
    app.scroll_from_bottom = app.scroll_from_bottom.min(max_scroll);

    // Bottom-anchored window: offset 0 shows the newest lines.
    let end = total - app.scroll_from_bottom;
    let start = end.saturating_sub(height);
    app.last_top_line = start;
    app.last_transcript_inner = inner;
    app.last_line_cells = rendered.cell_at;

    frame.render_widget(Paragraph::new(rendered.lines[start..end].to_vec()), inner);

    // The scrollbar rides the frame's right edge. Rendered only when there is
    // something to scroll a permanently full bar would read as
    // decoration.
    if total > height {
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None)
            .track_symbol(Some("│"))
            .track_style(theme.pane_border)
            .thumb_style(theme.scrollbar_thumb);
        let mut state = ScrollbarState::new(total).position(start);
        frame.render_stateful_widget(scrollbar, area, &mut state);
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
            Paragraph::new(marker).style(theme.scroll_marker),
            marker_area,
        );
    }
}

/// Wrapped question content and the rows belonging to the selected option. The range
/// includes its description, but not the blank row before it.
#[derive(Default)]
struct QuestionLines {
    lines: Vec<Line<'static>>,
    selected: std::ops::Range<usize>,
}

/// A frame-local render plan, not session state. Closing the question drops it.
struct QuestionPane {
    block: Block<'static>,
    content: QuestionLines,
    height: u16,
}

/// Construct the same block for measurement and painting. Small panes shed padding
/// first, then border, rather than reserving every row for chrome.
fn question_pane(app: &App, area: Rect, theme: &Theme) -> Option<QuestionPane> {
    app.question.as_ref()?;
    if area.is_empty() {
        return None;
    }
    let framed = area.width >= 3 && area.height >= 3;
    let horizontal = if area.width >= 12 {
        QUESTION_PADDING
    } else {
        0
    };
    let vertical = u16::from(area.height >= 7);
    let block = Block::new()
        .borders(if framed {
            Borders::TOP | Borders::BOTTOM
        } else {
            Borders::NONE
        })
        .border_style(theme.pane_border)
        .padding(Padding::new(horizontal, horizontal, vertical, vertical));
    let block = if framed {
        block.title(Span::styled(" Decision required ", theme.question))
    } else {
        block
    };
    let inner = block.inner(area);
    let content = question_lines(app, usize::from(inner.width.max(1)), theme);
    let chrome = area.height.saturating_sub(inner.height);
    let rows = u16::try_from(content.lines.len()).unwrap_or(u16::MAX);
    Some(QuestionPane {
        block,
        content,
        height: rows.saturating_add(chrome).min(area.height),
    })
}

/// Paint a viewport derived from the selection: no second scroll state can become
/// stale after Up/Down, a resize, or a switch to another session.
fn render_question(frame: &mut Frame<'_>, pane: QuestionPane, area: Rect, theme: &Theme) {
    let inner = pane.block.inner(area);
    let visible = usize::from(inner.height);
    let total = pane.content.lines.len();
    let selected = &pane.content.selected;
    // Fit the whole selected option when possible. If its description is taller than
    // the viewport, keep the label at the top instead of hidding the choice.
    let start = if selected.len() > visible {
        selected.start
    } else {
        selected.end.saturating_sub(visible)
    }
    .min(total.saturating_sub(visible));
    let end = (start + visible).min(total);
    let block = if area.width >= 3 && area.height >= 3 && visible > 0 && total > visible {
        pane.block.title_bottom(Span::styled(
            format!(" rows {}-{end}/{total} ", start + 1),
            theme.chrome,
        ))
    } else {
        pane.block
    };
    frame.render_widget(block, area);
    frame.render_widget(
        Paragraph::new(pane.content.lines[start..end].to_vec()),
        inner,
    );
}

/// Sanatize before wrapping and reserve the prefix's display columns on every row.
/// Continuations keep their indentation, including after explicit newlines.
fn push_question_text(
    lines: &mut Vec<Line<'static>>,
    text: &str,
    first: &str,
    continuation: &str,
    width: usize,
    style: Style,
) {
    let prefix_width = Line::from(first)
        .width()
        .max(Line::from(continuation).width());
    // On narrower terminals omit indentation; the Paragraph safely clips a character
    // wider than itself.
    let (first, continuation, prefix_width) = if width >= prefix_width + 2 {
        (first, continuation, prefix_width)
    } else {
        ("", "", 0)
    };
    let text = sanitize(text);
    let mut first_row = true;
    for logical in text.split('\n') {
        for row in transcript::wrap_line(logical, width.saturating_sub(prefix_width).max(1)) {
            let prefix = if first_row { first } else { continuation };
            lines.push(Line::from(Span::styled(format!("{prefix}{row}"), style)));
            first_row = false;
        }
    }
}

/// The question, a blank row, and separated options. All margins belong to the block;
/// prefixes here express hierarchy rather than outside padding.
fn question_lines(app: &App, width: usize, theme: &Theme) -> QuestionLines {
    let Some(open) = &app.question else {
        return QuestionLines::default();
    };
    let question = open.pending.question();
    let mut content = QuestionLines::default();
    push_question_text(
        &mut content.lines,
        &question.question,
        "? ",
        " ",
        width,
        theme.question,
    );
    for (index, choice) in question.options.iter().enumerate() {
        content.lines.push(Line::default());
        let start = content.lines.len();
        let (marker, style) = if index == open.selected {
            ("›", theme.popup_selected)
        } else {
            (" ", theme.popup_row)
        };
        let recommended = if choice.recommended {
            " (recommended)"
        } else {
            ""
        };
        push_question_text(
            &mut content.lines,
            &format!("{}{recommended}", choice.label),
            &format!("{marker} {}. ", index + 1),
            "     ",
            width,
            style,
        );
        if !choice.description.is_empty() {
            push_question_text(
                &mut content.lines,
                &choice.description,
                "     ",
                "     ",
                width,
                theme.chrome,
            );
        }
        if index == open.selected {
            content.selected = start..content.lines.len();
        }
    }
    content
}

/// The waiting prompts above the input box: "Steer:" rows first (they go in sooner),
/// then "Follow-up:" rows, oldest first. The first line of each prompt, then the key
/// that takes them back.
fn render_queued(frame: &mut Frame<'_>, app: &App, area: Rect, theme: &Theme) {
    if app.steering.is_empty() && app.queued.is_empty() && app.waiting_helper.is_none() {
        return;
    }
    let first_line = |text: &String| text.lines().next().unwrap_or_default().to_string();
    let lines: Vec<Line<'_>> = app
        .waiting_helper
        .iter()
        .map(|call| format!(" Agent: @{} {}", call.name, first_line(&call.task)))
        .chain(
            app.steering
                .iter()
                .map(|text| format!(" Steer: {}", first_line(text))),
        )
        .chain(
            app.queued
                .iter()
                .map(|text| format!(" Follow-up: {}", first_line(text))),
        )
        .chain([" ↳ alt+up to edit all queued messages".to_string()])
        .map(|line| Line::from(Span::styled(line, theme.chrome)))
        .collect();
    frame.render_widget(Paragraph::new(lines), area);
}

const SPINNER: [&str; 4] = ["◓", "◑", "◒", "◐"];

/// A pane frame: dim border, optional dim title, one column of horizontal padding.
fn pane_block(title: Option<&str>, theme: &Theme) -> Block<'static> {
    let block = Block::new()
        .borders(Borders::ALL)
        .border_style(theme.pane_border)
        .padding(Padding::horizontal(1));
    match title {
        Some(title) => block.title(Span::styled(format!(" {title} "), theme.chrome)),
        None => block,
    }
}

/// Cursor position as (visual line, visual column) in the wrapped input
/// text. Derived from the same `wrap_line` output that renders the text: a
/// second, parallel wrapping computation would inevitably disagree with it
/// (word wrap vs. plain column wrap) and paint the cursor away from where
/// the next keystroke actually lands.
///
/// `cursor` is a character index (see `InputState`). `wrap_line` preserves every
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
                // first char of the next chunk" inserting there joins the
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

fn render_input(frame: &mut Frame<'_>, app: &App, area: Rect, theme: &Theme) {
    let border_style = if app.is_running() {
        theme.input_border_busy
    } else {
        theme.input_border_idle
    };
    let mut block = Block::new()
        .borders(Borders::TOP | Borders::BOTTOM)
        .border_style(border_style)
        .padding(Padding::horizontal(INPUT_PADDING));
    if app.question.is_some() {
        block = block.title(Span::styled(" Your answer ", theme.chrome));
    } else if let Some(name) = &app.armed {
        block = block.title(Span::styled(
            format!(" next prompt → {name} (esc cancels) "),
            theme.chrome,
        ));
    } else if app.is_running() {
        let elapsed = app
            .run_started
            .map(|started| format!(" · {}", format_elapsed(started.elapsed())))
            .unwrap_or_default();
        let title = format!(
            " {} {}{elapsed} ",
            SPINNER[app.frame % SPINNER.len()],
            app.activity_label()
        );
        block = block.title(Span::styled(title, theme.chrome));
    }
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
    // outgrows the height-capped box.
    let (cursor_line, cursor_col) =
        visual_cursor(app.input.text(), app.input.cursor(), inner_width);
    let visible = inner.height.max(1) as usize;
    let scroll = cursor_line.saturating_sub(visible - 1);
    frame.render_widget(Paragraph::new(text).scroll((scroll as u16, 0)), inner);

    // Place the real terminal cursor at the editing position. (ratatui hides
    // it unless the app explicitly positions it each frame.)
    if app.usage_popup.is_none() {
        frame.set_cursor_position(Position {
            x: inner.x + (cursor_col as u16).min(inner.width.saturating_sub(1)),
            y: inner.y + ((cursor_line - scroll) as u16).min(inner.height.saturating_sub(1)),
        });
    }
}

fn render_model_status(frame: &mut Frame<'_>, app: &App, area: Rect, theme: &Theme) {
    // Only reasoning models show effort; keep "thinking" even for "off" so
    // the level never reads like part of the model name.
    let thinking_segment = if app.agent.model_supports_reasoning() {
        match app.agent.effective_thinking_level() {
            None => " | thinking off".to_string(),
            Some(level) => format!(" | thinking {}", thinking_level_name(level)),
        }
    } else {
        String::new()
    };
    let label = format!(
        "{} ({}){thinking_segment} ",
        app.meta.model_name, app.meta.provider,
    );
    frame.render_widget(
        Paragraph::new(Span::styled(label, theme.chrome)).right_aligned(),
        area,
    );
}

fn render_footer(frame: &mut Frame<'_>, app: &App, area: Rect, theme: &Theme) {
    let window = app.agent.context_window();
    let percent = (app.context_tokens * 100).checked_div(window).unwrap_or(0);
    let maximum = app.agent.max_context_window().filter(|&max| max > window);
    let window_label = if app.agent.context_window_is_assumed() {
        " assumed"
    } else if maximum.is_some() {
        " plan"
    } else {
        ""
    };
    let maximum_segment = maximum.map_or_else(String::new, |max| format!(" · max {}k", max / 1000));
    // The session id sits in the always-visible left half so `--resume
    // <id>` (or /hot-reload <id>) can be typed from what's on screen.
    let totals = app.session_totals();
    let left = format!(
        " {} | {} in / {} out / {} cached | ${:.4} | ctx {}k/{}k{window_label} ({percent}%){maximum_segment}",
        app.recorder.session_id(),
        totals.input,
        totals.output,
        totals.cache_read,
        totals.cost,
        app.context_tokens / 1000,
        window / 1000,
    );
    // Add current `cupel` version to footer.
    let version_label = format!("version: {}", env!("CARGO_PKG_VERSION"));
    // The mouse hint tracks selection mode, so it never lies about what
    // the wheel currently does. It shares the status row's left margin because
    // a right-aligned hint row would sit staggered against it.
    let hints = if app.question.is_some() {
        " ↑↓ choose · enter answer (typed text wins) · alt+enter newline · @ file · esc abort"
    } else if app.mouse_captured {
        " enter send · ctrl+enter steer · alt+enter newline · @ file · / cmds · esc abort · ctrl+o copy · ctrl+t tools · ctrl+y select"
    } else {
        " enter send · ctrl+enter steer · alt+enter newline · @ file · / cmds · esc abort · SELECTION MODE · ctrl+t tools · ctrl+y scroll"
    };

    let [status_row, hints_row] =
        Layout::vertical([Constraint::Length(1), Constraint::Length(1)]).areas(area);
    frame.render_widget(Paragraph::new(Span::styled(left, theme.chrome)), status_row);
    let [hints_area, version_area] = Layout::horizontal([
        Constraint::Min(0),
        Constraint::Length(version_label.len() as u16 + 1),
    ])
    .areas(hints_row);
    frame.render_widget(
        Paragraph::new(Span::styled(hints, theme.chrome)),
        hints_area,
    );
    // Right-aligned, the area's spare column sits in front of the label. It is
    // the gap that keeps cut-off hints from running into the version.
    frame.render_widget(
        Paragraph::new(Span::styled(version_label, theme.chrome)).right_aligned(),
        version_area,
    );
}

/// Display name for a thinking level the same lowercase words the
/// /thinking command accepts, so model status and command speak one language.
fn thinking_level_name(level: cupel_core::types::ThinkingLevel) -> &'static str {
    use cupel_core::types::ThinkingLevel;
    match level {
        ThinkingLevel::Minimal => "minimal",
        ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        ThinkingLevel::High => "high",
        ThinkingLevel::XHigh => "xhigh",
        ThinkingLevel::Max => "max",
    }
}

#[cfg(test)]
pub(crate) mod tests {
    //! Headless render tests: ratatui's `TestBackend` draws frames into an
    //! in-memory buffer, so the full render path is testable without a
    //! terminal (or an API key the Agent is constructed but never run).

    use super::*;
    use crate::theme::CUPEL;
    use crate::transcript::{Cell, ToolOutcome};
    use cupel_agent::{Agent, AgentEvent, AgentOptions};
    use cupel_coding_agent::modes::SessionMeta;
    use cupel_coding_agent::tools::ask_user::{Answer, AskUserTool, PendingQuestion};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use ratatui::style::Color;
    use ratatui::style::Modifier;
    use std::sync::Arc;

    pub(crate) fn test_app() -> App {
        test_app_in("/tmp")
    }

    /// App rooted at a specific cwd the autocomplete tests point this at
    /// a temp tree with known files.
    fn test_app_in(cwd: &str) -> App {
        let model = cupel_core::catalog::builtin_models().remove(0);
        let registry = Arc::new(cupel_core::provider::Registry::new());
        let agent = Agent::new(AgentOptions::new(model, registry));
        // home: None disables persistence + hooks tests touch no disk.
        let recorder = cupel_coding_agent::session::SessionRecorder::new(
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
                settings: cupel_coding_agent::settings::Settings::default(),
                home: None,
                startup_warning: None,
                warnings: Vec::new(),
                context_files: Vec::new(),
                base_system_prompt: String::new(),
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
        terminal
            .draw(|frame| render(frame, app, frame.area(), &CUPEL))
            .unwrap();
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
    /// then also its column. Panics when the needle is not on screen
    /// that is a failed test either way, and panicking here gives the
    /// missing-text message instead of a confusing style mismatch.
    fn style_of(app: &mut App, needle: &str) -> ratatui::style::Style {
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal
            .draw(|frame| render(frame, app, frame.area(), &CUPEL))
            .unwrap();
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

        let task = style_of(&mut app, "the task");
        assert_eq!(task.fg, Some(Color::White));
        assert_eq!(task.bg, Some(Color::Indexed(240)));

        assert_eq!(style_of(&mut app, "pondering").fg, Some(Color::White));

        let answer = style_of(&mut app, "the answer");
        assert_eq!(answer.fg, Some(Color::Magenta));
    }

    #[test]
    fn user_markdown_uses_a_gray_surface_without_changing_the_prompt() {
        let mut app = test_app();
        app.transcript.cells.push(Cell::User {
            text:
                "# User task\nplain **bold** and *italic* with `code`\n```rust\nfn main() {}\n```"
                    .into(),
        });
        app.input.insert_str("**draft**");
        let screen = draw(&mut app, 80, 24);
        assert!(screen.contains(">> User task"), "{screen}");
        assert!(
            screen.contains("plain bold and italic with code"),
            "{screen}"
        );
        assert!(screen.contains("fn main() {}"), "{screen}");
        assert!(
            !screen.contains("**bold**") && !screen.contains("```"),
            "{screen}"
        );
        for (text, modifier) in [
            ("User task", Modifier::BOLD),
            ("bold", Modifier::BOLD),
            ("italic", Modifier::ITALIC),
        ] {
            let style = style_of(&mut app, text);
            assert_eq!(style.fg, Some(Color::White));
            assert_eq!(style.bg, Some(Color::Indexed(240)));
            assert!(style.add_modifier.contains(modifier));
        }
        assert!(
            screen.contains("**draft**"),
            "the prompt stays raw:\n{screen}"
        );
        let prompt = style_of(&mut app, "**draft**");
        assert_eq!(prompt.fg, Some(Color::Reset));
        assert_eq!(prompt.bg, Some(Color::Reset));
        assert!(!prompt.add_modifier.contains(Modifier::BOLD));
        assert_eq!(style_of(&mut app, "fn main").fg, Some(Color::White));
        app.selected_cell = Some(0);
        assert_eq!(style_of(&mut app, "bold").bg, CUPEL.selected.bg);
        assert_eq!(style_of(&mut app, "fn main").bg, CUPEL.selected.bg);
    }

    #[test]
    fn prompt_editor_keeps_its_default_background_and_markdown_source() {
        let mut app = test_app();
        app.input.insert_str("**draft**");
        let mut terminal = Terminal::new(TestBackend::new(20, 3)).unwrap();
        terminal
            .draw(|frame| render_input(frame, &app, frame.area(), &CUPEL))
            .unwrap();
        assert_eq!(
            terminal.get_cursor_position().unwrap(),
            Position::new(10, 1)
        );
        let buffer = terminal.backend().buffer();
        for x in 0..20 {
            let cell = &buffer[(x, 1)];
            assert_eq!(cell.fg, Color::Reset);
            assert_eq!(cell.bg, Color::Reset);
            assert!(!cell.modifier.contains(Modifier::BOLD));
        }
        let text: String = (1..10).map(|x| buffer[(x, 1)].symbol()).collect();
        assert_eq!(text, "**draft**", "editing shows the raw source");
    }

    #[test]
    fn prompt_wrapping_and_cursor_use_the_same_inner_width() {
        let mut app = test_app();
        app.input.insert_str("hello brave new world");
        let mut terminal = Terminal::new(TestBackend::new(20, 20)).unwrap();
        terminal
            .draw(|frame| render(frame, &mut app, frame.area(), &CUPEL))
            .unwrap();
        // Two border rows plus two text rows: input starts at y=14.
        assert_eq!(
            terminal.get_cursor_position().unwrap(),
            Position::new(6, 16)
        );
        let buffer = terminal.backend().buffer();
        let first: String = (1..17).map(|x| buffer[(x, 15)].symbol()).collect();
        let second: String = (1..6).map(|x| buffer[(x, 16)].symbol()).collect();
        assert_eq!(first, "hello brave new ");
        assert_eq!(second, "world");
    }

    #[test]
    fn prompt_fills_the_padded_width_without_growing_an_extra_row() {
        let mut app = test_app();
        app.input.insert_str("abcdefghijklmnopqr");
        let mut terminal = Terminal::new(TestBackend::new(20, 20)).unwrap();
        terminal
            .draw(|frame| render(frame, &mut app, frame.area(), &CUPEL))
            .unwrap();
        let buffer = terminal.backend().buffer();
        let text: String = (0..20).map(|x| buffer[(x, 16)].symbol()).collect();
        assert_eq!(text, " abcdefghijklmnopqr ");
        assert_eq!(
            terminal.get_cursor_position().unwrap(),
            Position::new(18, 16)
        );
        assert_eq!(app.last_transcript_inner.height, 12);
    }

    #[test]
    fn prompt_padding_applies_to_empty_input_and_wrapped_lines() {
        let mut app = test_app();
        let mut terminal = Terminal::new(TestBackend::new(20, 20)).unwrap();
        terminal
            .draw(|frame| render(frame, &mut app, frame.area(), &CUPEL))
            .unwrap();
        assert_eq!(
            terminal.get_cursor_position().unwrap(),
            Position::new(1, 16)
        );

        app.input.insert_str("abcdefghijklmnopqrst");
        terminal
            .draw(|frame| render(frame, &mut app, frame.area(), &CUPEL))
            .unwrap();
        assert_eq!(
            terminal.get_cursor_position().unwrap(),
            Position::new(3, 16)
        );
        assert_eq!(
            app.last_transcript_inner.height, 11,
            "wrapped input gets two rows"
        );
        let buffer = terminal.backend().buffer();
        let first: String = (0..20).map(|x| buffer[(x, 15)].symbol()).collect();
        let second: String = (0..20).map(|x| buffer[(x, 16)].symbol()).collect();
        assert_eq!(first, " abcdefghijklmnopqr ");
        assert_eq!(second, format!(" st{}", " ".repeat(17)));
    }

    #[test]
    fn only_the_prompt_has_top_and_bottom_borders_without_a_title() {
        let mut app = test_app();
        app.input.insert_str("draft");
        app.transcript.cells.push(Cell::User {
            text: "task".into(),
        });
        let screen = draw(&mut app, 80, 20);
        assert!(!screen.contains(" conversation "), "{screen}");
        assert!(!screen.contains(" prompt "), "{screen}");
        for (y, row) in screen.lines().enumerate() {
            if matches!(y, 15 | 17) {
                assert_eq!(row, "─".repeat(80), "{screen}");
            } else {
                assert!(!row.contains('─'), "{screen}");
            }
        }
        assert!(!screen.contains(['│', '┌', '┐', '└', '┘']), "{screen}");
        assert!(
            screen.lines().nth(1).unwrap().starts_with(" >> task"),
            "{screen}"
        );
        assert!(screen.lines().next().unwrap().trim().is_empty(), "{screen}");
        assert!(
            screen.lines().nth(13).unwrap().trim().is_empty(),
            "{screen}"
        );
        assert!(
            screen.lines().nth(16).unwrap().starts_with(" draft"),
            "{screen}"
        );
        assert_eq!(app.last_transcript_inner, Rect::new(1, 1, 78, 12));
        assert_eq!(style_of(&mut app, "─").fg, CUPEL.input_border_idle.fg);
    }

    #[test]
    fn conversation_padding_remains_when_the_transcript_overflows() {
        let mut app = test_app();
        app.transcript.cells = vec![Cell::Answer {
            text: (1..=20)
                .map(|n| format!("line {n}"))
                .collect::<Vec<_>>()
                .join("\n"),
        }];
        let screen = draw(&mut app, 80, 20);
        assert_eq!(app.last_transcript_inner, Rect::new(1, 1, 78, 12));
        assert!(
            screen.lines().nth(1).unwrap().starts_with(" line 9"),
            "{screen}"
        );
        assert!(
            screen.lines().nth(12).unwrap().starts_with(" line 20"),
            "{screen}"
        );
        for y in [0, 13] {
            // The scrollbar may occupy the right edge; text stays inside the padding.
            assert!(
                screen
                    .lines()
                    .nth(y)
                    .unwrap()
                    .chars()
                    .take(79)
                    .all(|c| c == ' '),
                "{screen}"
            );
        }
    }

    #[test]
    fn prompt_handles_small_terminals() {
        for (width, height) in [(6, 6), (10, 10), (20, 12)] {
            let mut app = test_app();
            app.input.insert_str("a long draft on a small terminal");
            let _ = draw(&mut app, width, height);
        }
    }

    #[test]
    fn startup_warning_leads_the_transcript_as_a_notice() {
        let model = cupel_core::catalog::builtin_models().remove(0);
        let registry = Arc::new(cupel_core::provider::Registry::new());
        let recorder = cupel_coding_agent::session::SessionRecorder::new(
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
                settings: cupel_coding_agent::settings::Settings::default(),
                home: None,
                startup_warning: Some(
                    "no credentials found - use /provider <name> <api-key>".into(),
                ),
                warnings: Vec::new(),
                context_files: Vec::new(),
                base_system_prompt: String::new(),
            },
            recorder,
        );
        // First cell is the warning notice the session is usable, not
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
    fn configuration_warnings_render_as_notices_once() {
        let app = test_app();
        let mut meta = app.meta;
        meta.warnings = vec![
            "warning: ignoring settings file: broken JSON".into(),
            "warning: ignoring invalid bash-deny pattern".into(),
        ];
        let mut app = App::new(app.agent, meta, app.recorder);
        assert!(app.meta.warnings.is_empty(), "notices consume the warnings");
        for _ in 0..2 {
            let screen = draw(&mut app, 100, 20);
            assert!(screen.contains("ignoring settings file"), "{screen}");
            assert!(screen.contains("invalid bash-deny pattern"), "{screen}");
        }
        assert_eq!(app.transcript.cells.len(), 2);
        assert!(
            app.transcript
                .cells
                .iter()
                .all(|cell| matches!(cell, Cell::Notice { .. }))
        );
        assert!(
            app.agent.state().messages.is_empty(),
            "warnings are UI-only"
        );
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
    fn footer_omits_run_status() {
        for running in [false, true] {
            let mut app = test_app();
            let (events, _sink) = cupel_agent::agent_loop::agent_event_channel();
            app.run_events = running.then_some(events);
            for width in [80, 120, 200] {
                let screen = draw(&mut app, width, 20);
                let status_row = screen.lines().nth(18).expect("footer status row");
                assert!(
                    status_row
                        .starts_with(" cupel-test | 0 in / 0 out / 0 cached | $0.0000 | ctx "),
                    "footer must show session and usage without run status:\n{screen}"
                );
            }
        }
    }

    #[test]
    fn footer_ends_the_hints_row_with_the_version_after_a_gap() {
        let mut app = test_app();
        // The leading space is the gap: at 80 and 140 columns the hints are cut
        // off, and the cut must not touch the label.
        let version_suffix = format!(" version: {}", env!("CARGO_PKG_VERSION"));
        for mouse_captured in [true, false] {
            app.mouse_captured = mouse_captured;
            for width in [80, 140, 200] {
                let screen = draw(&mut app, width, 20);
                let hints_row = screen.lines().last().expect("hints row");
                assert!(hints_row.starts_with(" enter send"), "{screen}");
                assert!(hints_row.ends_with(&version_suffix), "{screen}");
                if width == 200 {
                    let mouse_hint = if mouse_captured {
                        "ctrl+y select"
                    } else {
                        "ctrl+y scroll"
                    };
                    assert!(hints_row.contains(mouse_hint), "{screen}");
                }
            }
        }
    }

    #[test]
    fn session_id_command_lists_this_projects_sessions() {
        // A home-backed recorder (unlike test_app's disabled one) so the
        // listing has a real sessions dir to read.
        let root = std::env::temp_dir().join("cupel-ui-session-list");
        let _ = std::fs::remove_dir_all(&root);
        let (home, cwd) = (root.join("home"), root.join("proj"));
        std::fs::create_dir_all(&cwd).unwrap();
        let recorder = cupel_coding_agent::session::SessionRecorder::new(
            Some(home),
            &cwd,
            "cupel-current",
            "test-model",
        );
        // Pre-write an older session the listing must show alongside.
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
                settings: cupel_coding_agent::settings::Settings::default(),
                home: None,
                startup_warning: None,
                warnings: Vec::new(),
                context_files: Vec::new(),
                base_system_prompt: String::new(),
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

    #[test]
    fn session_id_command_rejects_arguments() {
        for args in [
            "random-string",
            "cupel-9999999999999",
            "cupel-test",
            "random string",
            "  random-string  ",
        ] {
            let mut app = test_app();
            submit_command(&mut app, &format!("/session-id {args}"));

            let [Cell::Notice { text }] = app.transcript.cells.as_slice() else {
                panic!("expected only an invalid-argument notice for {args:?}");
            };
            assert!(text.contains("invalid argument for /session-id"), "{text}");
            assert!(text.contains(&format!("{:?}", args.trim())), "{text}");
            assert!(text.contains("takes no arguments"), "{text}");
            assert!(text.contains("/hot-reload <session-id>"), "{text}");
            assert!(!text.contains("current session:"), "{text}");
            assert!(app.pending_prompt.is_none());
            assert!(app.pending_reload.is_none());
            assert!(!app.is_running());
        }
    }

    #[test]
    fn session_id_command_accepts_trailing_whitespace() {
        for command in ["/session-id", "/session-id   ", "/session-id\t"] {
            let mut app = test_app();
            submit_command(&mut app, command);

            let [Cell::Notice { text }] = app.transcript.cells.as_slice() else {
                panic!("expected only a session-listing notice for {command:?}");
            };
            assert!(text.contains("current session: cupel-test"), "{text}");
            assert!(text.contains("session persistence is disabled"), "{text}");
        }
    }

    /// App with a real home + cwd on disk, for reload/resume tests.
    pub(crate) fn test_app_with_home(root: &std::path::Path, session_id: &str) -> App {
        let (home, cwd) = (root.join("home"), root.join("proj"));
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        let recorder = cupel_coding_agent::session::SessionRecorder::new(
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
                settings: cupel_coding_agent::settings::Settings::default(),
                home: Some(home),
                startup_warning: None,
                warnings: Vec::new(),
                context_files: Vec::new(),
                base_system_prompt: String::new(),
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
        assert_eq!(app.pending_reload, Some(crate::app::ReloadTarget::Current));

        let mut app = test_app();
        type_text(&mut app, "/hot-reload cupel-42");
        app.on_terminal_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
        app.on_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
        assert_eq!(
            app.pending_reload,
            Some(crate::app::ReloadTarget::Resume("cupel-42".into()))
        );
    }

    #[test]
    fn new_command_requests_a_rebuild_only_when_idle() {
        let mut app = test_app();
        run_command(&mut app, "/new");
        assert_eq!(app.pending_reload, Some(crate::app::ReloadTarget::New));
        assert_eq!(app.recorder.session_id(), "cupel-test", "rebuild is async");

        app.pending_reload = None;
        let (events, _sink) = cupel_agent::agent_loop::agent_event_channel();
        app.run_events = Some(events);
        run_command(&mut app, "/new");
        assert!(app.pending_reload.is_none());
        assert_eq!(app.recorder.session_id(), "cupel-test");
        assert!(app.transcript.cells.iter().any(|cell| matches!(
            cell,
            Cell::Notice { text } if text.contains("cannot clear while the agent is working")
        )));
    }

    #[tokio::test]
    async fn new_session_rotates_transcripts_without_restoring_old_history() {
        use cupel_coding_agent::session::{find_latest, load_transcript};

        let root = std::env::temp_dir().join("cupel-ui-new-session");
        let _ = std::fs::remove_dir_all(&root);
        let mut app = test_app_with_home(&root, "cupel-old");
        let old_message = cupel_agent::AgentMessage::user_text("old conversation");
        let mut options = AgentOptions::new(app.agent.state().model, app.agent.registry());
        options.session_id = Some("cupel-old".into());
        options.messages = vec![old_message.clone()];
        app.agent = Agent::new(options);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            for event in ["session-start", "stop", "session-end"] {
                let dir = root.join("home/hooks").join(event);
                std::fs::create_dir_all(&dir).unwrap();
                let script = dir.join("record");
                std::fs::write(
                    &script,
                    "#!/bin/sh\ncat >> lifecycle.jsonl\nprintf '\\n' >> lifecycle.jsonl\n",
                )
                .unwrap();
                std::fs::set_permissions(script, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
        }

        app.recorder.before_prompt("old conversation").await;
        app.on_agent_event(Some(AgentEvent::MessageEnd {
            message: old_message.clone(),
        }))
        .await;
        app.recorder.on_agent_end();
        app.agent
            .follow_up(cupel_agent::AgentMessage::user_text("old queued prompt"));
        app.queued.push("old queued prompt".into());
        app.selected_cell = Some(0);
        app.totals.input = 100;
        app.context_tokens = 100;
        app.scroll_from_bottom = 10;
        let dir = app.recorder.sessions_dir().unwrap().to_path_buf();
        let old_path = dir.join("cupel-old.jsonl");
        let old_contents = std::fs::read_to_string(&old_path).unwrap();

        run_command(&mut app, "/new");
        // Mirror the event loop's async handling of a requested rebuild.
        if let Some(target) = app.pending_reload.take() {
            app = app.hot_reload(target).await;
        }
        let new_id = app.recorder.session_id().to_string();
        assert_ne!(new_id, "cupel-old");
        assert!(app.agent.state().messages.is_empty());
        assert!(app.agent.take_follow_up().is_none());
        assert!(app.queued.is_empty());
        assert!(app.selected_cell.is_none());
        assert_eq!(app.totals.input, 0);
        assert_eq!(app.context_tokens, 0);
        assert_eq!(app.scroll_from_bottom, 0);
        assert!(
            !app.transcript
                .cells
                .iter()
                .any(|cell| matches!(cell, Cell::User { .. }))
        );
        let new_path = dir.join(format!("{new_id}.jsonl"));
        assert!(
            !new_path.exists(),
            "new transcript stays lazy until a prompt"
        );

        // Further work belongs only to the fresh session. Ensure its mtime
        // differs even on filesystems with millisecond timestamp resolution.
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        let new_message = cupel_agent::AgentMessage::user_text("new conversation");
        app.recorder.before_prompt("new conversation").await;
        app.on_agent_event(Some(AgentEvent::MessageEnd {
            message: new_message.clone(),
        }))
        .await;
        app.recorder.end_session().await;
        drop(app);

        assert_eq!(std::fs::read_to_string(&old_path).unwrap(), old_contents);
        assert_eq!(load_transcript(&old_path).unwrap().1, vec![old_message]);
        let latest = find_latest(Some(&root.join("home")), &root.join("proj")).unwrap();
        assert_eq!(latest, new_path, "bare --resume selects the new session");
        let (header, messages) = load_transcript(&latest).unwrap();
        assert_eq!(header.session_id, new_id);
        assert_eq!(messages, vec![new_message]);

        #[cfg(unix)]
        {
            let events: Vec<serde_json::Value> =
                std::fs::read_to_string(root.join("proj/lifecycle.jsonl"))
                    .unwrap()
                    .lines()
                    .map(|line| serde_json::from_str(line).unwrap())
                    .collect();
            let expected = [
                ("session-start", "cupel-old"),
                ("stop", "cupel-old"),
                ("session-end", "cupel-old"),
                ("session-start", new_id.as_str()),
                ("session-end", new_id.as_str()),
            ];
            assert_eq!(events.len(), expected.len());
            for (event, (name, id)) in events.iter().zip(expected) {
                assert_eq!(event["event"], name);
                assert_eq!(event["sessionId"], id);
                assert_eq!(
                    event["sessionRef"],
                    dir.join(format!("{id}.jsonl")).display().to_string()
                );
            }
        }
    }

    #[tokio::test]
    async fn new_session_preserves_runtime_configuration() {
        let root = std::env::temp_dir().join("cupel-ui-new-runtime");
        let _ = std::fs::remove_dir_all(&root);
        let mut app = test_app_with_home(&root, "cupel-current");
        let model = acme_model();
        app.agent.set_model(model.clone());
        app.agent
            .set_thinking_level(Some(cupel_core::types::ThinkingLevel::High));
        app.meta.base_system_prompt = "old base".into();
        app.agent
            .set_system_prompt("old base\n\nPRESET RULES".into());
        app.session_keys.insert("acme".into(), "entered-key".into());
        app.mouse_captured = false;
        std::fs::write(
            root.join("home/settings.json"),
            r#"{"providers": {"acme": "from-disk"}}"#,
        )
        .unwrap();
        std::fs::write(root.join("home/AGENTS.md"), "FRESH CONTEXT").unwrap();

        run_command(&mut app, "/new");
        let target = app.pending_reload.take().expect("new session requested");
        let app = app.hot_reload(target).await;
        let state = app.agent.state();
        assert_eq!(state.model, model);
        assert_eq!(
            state.thinking_level,
            Some(cupel_core::types::ThinkingLevel::High)
        );
        assert_eq!(
            state.system_prompt,
            format!("{}\n\nPRESET RULES", app.meta.base_system_prompt)
        );
        assert!(state.system_prompt.contains("FRESH CONTEXT"));
        assert_eq!(app.agent.api_key(), Some("entered-key"));
        assert_eq!(
            app.session_keys.get("acme").map(String::as_str),
            Some("entered-key")
        );
        assert_eq!(app.meta.settings.api_key("acme"), Some("from-disk"));
        assert!(!app.mouse_captured);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn hot_reload_current_preserves_recorder_lifecycle_and_pending_hooks() {
        use crate::app::ReloadTarget;
        use std::os::unix::fs::PermissionsExt as _;
        use std::time::Duration;

        for next_prompt in [false, true] {
            let root =
                std::env::temp_dir().join(format!("cupel-ui-hotreload-lifecycle-{next_prompt}"));
            let _ = std::fs::remove_dir_all(&root);
            let mut app = test_app_with_home(&root, "cupel-current");
            for event in ["session-start", "user-prompt-submit", "stop", "session-end"] {
                let dir = root.join("home/hooks").join(event);
                std::fs::create_dir_all(&dir).unwrap();
                let script = dir.join("record");
                let gate = if event == "stop" {
                    ": > stop-started\nwhile [ ! -e release-stop ]; do sleep 0.01; done\n"
                } else {
                    ""
                };
                std::fs::write(
                    &script,
                    format!(
                        "#!/bin/sh\n{gate}cat >> lifecycle.jsonl\nprintf '\\n' >> lifecycle.jsonl\n"
                    ),
                )
                .unwrap();
                std::fs::set_permissions(script, std::fs::Permissions::from_mode(0o755)).unwrap();
            }

            app.recorder.before_prompt("first prompt").await;
            app.recorder.on_agent_end();
            tokio::time::timeout(Duration::from_secs(5), async {
                while !root.join("proj/stop-started").exists() {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("stop hook started");

            let mut app = app.hot_reload(ReloadTarget::Current).await;
            assert_eq!(app.recorder.session_id(), "cupel-current");
            let mut completion = tokio::spawn(async move {
                if next_prompt {
                    app.recorder.before_prompt("next prompt").await;
                }
                app.recorder.end_session().await;
            });
            let waiting_for_stop =
                tokio::time::timeout(Duration::from_millis(100), &mut completion)
                    .await
                    .is_err();
            std::fs::write(root.join("proj/release-stop"), "").unwrap();
            if waiting_for_stop {
                tokio::time::timeout(Duration::from_secs(5), completion)
                    .await
                    .expect("hook chain settled")
                    .unwrap();
            }
            assert!(waiting_for_stop, "reload must retain the pending stop hook");

            let events: Vec<serde_json::Value> =
                std::fs::read_to_string(root.join("proj/lifecycle.jsonl"))
                    .unwrap()
                    .lines()
                    .map(|line| serde_json::from_str(line).unwrap())
                    .collect();
            let mut expected = vec!["session-start", "user-prompt-submit", "stop"];
            if next_prompt {
                expected.push("user-prompt-submit");
            }
            expected.push("session-end");
            assert_eq!(events.len(), expected.len(), "one session lifecycle");
            for (event, name) in events.iter().zip(expected) {
                assert_eq!(event["event"], name, "hook order survives reload");
                assert_eq!(event["sessionId"], "cupel-current");
            }
        }
    }

    #[tokio::test]
    async fn hot_reload_current_appends_only_the_agents_delta() {
        use crate::app::ReloadTarget;
        use cupel_coding_agent::resources::{CONTEXT_UPDATE_MARKER, ContextFile};

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
        // Some history that must survive the reload.
        app.agent = {
            let model = cupel_core::catalog::builtin_models().remove(0);
            let registry = Arc::new(cupel_core::provider::Registry::new());
            let mut options = AgentOptions::new(model, registry);
            options.messages = vec![cupel_agent::AgentMessage::user_text("earlier prompt")];
            Agent::new(options)
        };

        // Edit one rule mid-session.
        let edited = original.replace("RULE 6", "RULE 6 (amended)");
        std::fs::write(root.join("home/AGENTS.md"), &edited).unwrap();

        let app = app.hot_reload(ReloadTarget::Current).await;

        // The session continues: same id, history intact.
        assert_eq!(app.recorder.session_id(), "cupel-current");
        let messages = app.agent.state().messages;
        assert_eq!(messages.len(), 2, "history + appended delta");
        // The system prompt was not rebuilt (test agent starts with an
        // empty one re-embedding would have injected the rules).
        assert!(!app.agent.state().system_prompt.contains("RULE"));
        // The appended message is the delta, not the whole file: the
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

        // Reload again with no further edits: nothing new is appended.
        let app = app.hot_reload(ReloadTarget::Current).await;
        assert_eq!(app.agent.state().messages.len(), 2, "no duplicate delta");
        assert!(app.transcript.cells.iter().any(|c| matches!(
            c,
            Cell::Notice { text } if text.contains("no context file changes")
        )));
    }

    #[tokio::test]
    async fn hot_reload_resumes_a_session_by_id_and_rejects_unknown_ids() {
        use crate::app::ReloadTarget;

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
            call: "grep /bug/ in src".into(),
            expanded: false,
            started_at: None,
            live: None,
            result: Some(ToolOutcome {
                text: "src/main.rs:1: bug".into(),
                is_error: false,
                diff: None,
                took: None,
            }),
        });
        app.input.insert_str("next question");

        let screen = draw(&mut app, 80, 20);
        assert!(
            screen.contains("find the bug"),
            "user cell missing:\n{screen}"
        );
        assert!(
            screen.contains("grep /bug/ in src"),
            "tool cell missing:\n{screen}"
        );
        assert!(
            screen.contains("src/main.rs:1: bug"),
            "tool result missing:\n{screen}"
        );
        assert!(screen.contains("next question"), "input missing:\n{screen}");
        assert!(
            screen.contains("Test Model"),
            "model status missing:\n{screen}"
        );
        assert!(
            screen.contains("cupel-test | 0 in"),
            "footer missing:\n{screen}"
        );
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
    fn scrolled_view_stays_pinned_while_output_grows() {
        let mut app = test_app();
        for i in 0..30 {
            app.transcript.cells.push(Cell::Assistant {
                text: format!("line {i}"),
            });
        }
        // Learn the geometry, then scroll up into history.
        let _ = draw(&mut app, 40, 12);
        app.on_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::PageUp,
            KeyModifiers::NONE,
        )));
        let _ = draw(&mut app, 40, 12);
        let anchor = app.last_top_line;

        // The agent keeps streaming below the window.
        for i in 0..8 {
            app.transcript.cells.push(Cell::Assistant {
                text: format!("tail {i}"),
            });
        }
        let after = draw(&mut app, 40, 12);

        // The window held its place in content space: same top line, and
        // none of the new tail scrolled into view.
        assert_eq!(app.last_top_line, anchor, "view must stay pinned");
        assert!(
            !after.contains("tail"),
            "new output must not slide the view:\n{after}"
        );

        // Scrolling back down resumes following the tail as before.
        for _ in 0..100 {
            app.on_terminal_event(Event::Key(KeyEvent::new(
                KeyCode::PageDown,
                KeyModifiers::NONE,
            )));
        }
        let followed = draw(&mut app, 40, 12);
        assert!(
            followed.contains("tail 7"),
            "follow mode resumes:\n{followed}"
        );
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
        assert!(
            screen.lines().any(|row| row.starts_with("  src/main.rs ")),
            "popup starts at the input's padded left edge:\n{screen}"
        );
        assert!(
            screen
                .lines()
                .nth(14)
                .unwrap()
                .ends_with("Test Model (test) | thinking off "),
            "autocomplete must not cover the model status:\n{screen}"
        );
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

        // /exit is not a command: it is neither offered nor handled locally.
        let mut app = test_app();
        type_text(&mut app, "/ex");
        assert!(app.autocomplete.visible().is_none());
        type_text(&mut app, "it");
        assert!(app.autocomplete.visible().is_none());
        app.on_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
        assert!(!app.should_quit);
        assert_eq!(app.pending_prompt.as_deref(), Some("/exit"));
    }

    #[test]
    fn visual_cursor_matches_word_wrapping() {
        // width 10 word-wraps "hello world" as ["hello ", "world"]. A plain
        // column-wrap computation would report (1, 1) here the regression
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
                AssistantContent::ToolCall(ToolCall {
                    id: "call_2".into(),
                    name: "apply_patch".into(),
                    arguments: serde_json::json!({"input": "*** Begin Patch\n*** Update File: src/main.rs\n@@\n-bug();\n+fix();\n*** End Patch"}),
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
            error_kind: None,
            timestamp: now_ms(),
        };
        let patch_main = ToolResultMessage {
            tool_call_id: "call_2".into(),
            tool_name: "apply_patch".into(),
            content: vec![cupel_core::types::ToolResultContent::Text(
                TextContent::plain("Success. Updated the following files:\nM src/main.rs"),
            )],
            details: Some(serde_json::json!({"diff": "-1 bug();\n+1 fix();"})),
            is_error: false,
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
            AgentMessage::Llm(Message::ToolResult(patch_main)),
        ];
        let agent = Agent::new(options);
        let recorder = cupel_coding_agent::session::SessionRecorder::new(
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
                settings: cupel_coding_agent::settings::Settings::default(),
                home: None,
                startup_warning: None,
                warnings: Vec::new(),
                context_files: Vec::new(),
                base_system_prompt: String::new(),
            },
            recorder,
        );

        // Leave room for the replayed history and the padded user message.
        let screen = draw(&mut app, 80, 28);
        assert!(
            screen.contains("resumed session cupel-resumed (4 messages)"),
            "resume notice missing:\n{screen}"
        );
        assert!(screen.contains("old question"), "user cell:\n{screen}");
        assert!(screen.contains("the answer"), "assistant cell:\n{screen}");
        assert!(
            screen.contains("src/main.rs:1: bug"),
            "tool result attached:\n{screen}"
        );
        assert!(screen.contains("+1 fix();"), "diff replayed:\n{screen}");
        assert!(!screen.contains("Updated the following"), "{screen}");
    }

    #[test]
    fn ctrl_t_expands_every_tool_result_and_a_click_expands_one() {
        use ratatui::crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

        let mut app = test_app();
        app.transcript.append_thinking("looking");
        for id in ["1", "2"] {
            app.transcript.cells.push(Cell::Tool {
                id: id.into(),
                name: "read".into(),
                call: format!("read file{id}.rs"),
                expanded: false,
                started_at: None,
                live: None,
                result: Some(ToolOutcome {
                    text: (1..=9)
                        .map(|i| format!("row {i}"))
                        .collect::<Vec<_>>()
                        .join("\n"),
                    is_error: false,
                    diff: None,
                    took: None,
                }),
            });
        }
        // Wide enough that the marker line renders unwrapped.
        let screen = draw(&mut app, 120, 40);
        assert!(
            screen.contains("... (3 more lines, ctrl+t to expand)"),
            "{screen}"
        );
        assert!(
            !screen.contains("row 9"),
            "hidden while collapsed:\n{screen}"
        );

        // A click on the first tool's header expands just that cell.
        let line = app
            .last_line_cells
            .iter()
            .position(|cell| *cell == Some(1))
            .expect("first tool mapped");
        let row = app.last_transcript_inner.y + (line - app.last_top_line) as u16;
        app.on_terminal_event(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: app.last_transcript_inner.x + 1,
            row,
            modifiers: KeyModifiers::NONE,
        }));
        assert!(matches!(
            app.transcript.cells[1],
            Cell::Tool { expanded: true, .. }
        ));
        assert!(matches!(
            app.transcript.cells[2],
            Cell::Tool {
                expanded: false,
                ..
            }
        ));
        assert_eq!(
            app.selected_cell, None,
            "a tool click toggles, it never selects"
        );
        let screen = draw(&mut app, 120, 40);
        assert_eq!(screen.matches("row 9").count(), 1, "{screen}");

        // Ctrl+T: everything, then nothing.
        app.on_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::Char('t'),
            KeyModifiers::CONTROL,
        )));
        let screen = draw(&mut app, 120, 40);
        assert_eq!(screen.matches("row 9").count(), 2, "{screen}");
        assert!(!screen.contains("expand"), "{screen}");
        app.on_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::Char('t'),
            KeyModifiers::CONTROL,
        )));
        let screen = draw(&mut app, 120, 40);
        assert!(!screen.contains("row 9"), "{screen}");
    }

    /// A finished assistant message for event tests.
    fn assistant_message(
        text: &str,
        stop_reason: cupel_core::types::StopReason,
        total_tokens: u64,
    ) -> cupel_agent::AgentMessage {
        use cupel_core::types::{Api, AssistantContent, TextContent, Usage, now_ms};
        cupel_agent::AgentMessage::Llm(cupel_core::types::Message::Assistant(
            cupel_core::types::AssistantMessage {
                content: vec![AssistantContent::Text(TextContent::plain(text))],
                api: Api::from("mock"),
                provider: cupel_core::types::Provider::from("mock"),
                model: "mock".into(),
                response_model: None,
                response_id: None,
                usage: Usage {
                    total_tokens,
                    ..Usage::default()
                },
                stop_reason,
                error_message: None,
                error_kind: None,
                timestamp: now_ms(),
            },
        ))
    }

    #[tokio::test]
    async fn the_usage_line_counts_reasoning_tokens() {
        let mut app = test_app();
        let mut message = assistant_message("ok", cupel_core::types::StopReason::Stop, 0);
        if let cupel_agent::AgentMessage::Llm(cupel_core::types::Message::Assistant(assistant)) =
            &mut message
        {
            assistant.usage.output = 400;
            assistant.usage.reasoning = Some(312);
        }
        app.on_agent_event(Some(AgentEvent::MessageEnd { message }))
            .await;
        assert!(matches!(
            app.transcript.cells.last(),
            Some(Cell::Usage { text }) if text == "[0 in / 400 out (312 reasoning) / 0 cached, $0.0000]"
        ));
    }

    #[tokio::test]
    async fn thinking_off_says_when_the_model_cannot_turn_it_off() {
        // How many times the clamp was announced so far.
        fn clamp_notices(app: &App) -> usize {
            app.transcript
                .cells
                .iter()
                .filter(|cell| {
                    matches!(cell, Cell::Notice { text } if text.contains("cannot turn thinking off"))
                })
                .count()
        }
        // The test app starts with thinking off on Claude Sonnet 5, which can
        // turn reasoning off: nothing to say.
        let mut app = test_app();
        assert_eq!(clamp_notices(&app), 0);

        submit_command(&mut app, "/model codex/gpt-6.1-sol");
        assert!(has_notice(
            &app,
            "GPT-6.1 Sol cannot turn thinking off, using minimal"
        ));
        // The footer shows the level that is really used.
        assert!(draw(&mut app, 200, 20).contains("thinking minimal"));
        // Asking for off again says it again.
        submit_command(&mut app, "/thinking off");
        assert_eq!(clamp_notices(&app), 2);

        // Back on a model that can turn reasoning off, off is off again.
        submit_command(&mut app, "/model claude-sonnet-5");
        assert!(draw(&mut app, 200, 20).contains("thinking off"));
        assert_eq!(clamp_notices(&app), 2);
    }

    #[tokio::test]
    async fn footer_shows_context_usage_after_a_turn() {
        let mut app = test_app();
        assert!(
            draw(&mut app, 200, 20).contains("ctx 0k/"),
            "meter starts empty"
        );
        app.on_agent_event(Some(AgentEvent::MessageEnd {
            message: assistant_message("ok", cupel_core::types::StopReason::Stop, 48_000),
        }))
        .await;
        let screen = draw(&mut app, 200, 20);
        let window = app.agent.context_window();
        assert!(
            screen.contains(&format!(
                "ctx 48k/{}k ({}%)",
                window / 1000,
                48_000 * 100 / window
            )),
            "{screen}"
        );
        // A compaction resets the meter to the post-compaction estimate.
        app.on_agent_event(Some(AgentEvent::CompactionEnd {
            tokens_before: 48_000,
            tokens_after: 12_000,
            error: None,
            summary: None,
            messages: Vec::new(),
        }))
        .await;
        assert!(draw(&mut app, 200, 20).contains("ctx 12k/"));
    }

    #[test]
    fn footer_distinguishes_planning_window_from_maximum_after_model_switches() {
        let mut app = test_app();
        app.context_tokens = 48_000;
        // The windows come from models.dev through the catalog, so they are
        // read from the rows here, never pinned: a `--fetch` that corrects
        // one must not break this test.
        let catalog = cupel_core::catalog::builtin_models();
        let row = |id: &str| catalog.iter().find(|m| m.id == id).expect(id).clone();

        for id in ["gpt-6.1-sol", "codex/gpt-6.1-sol"] {
            let model = row(id);
            let max = model
                .max_context_window
                .expect("a planning row has a ceiling");
            assert!(max > model.context_window, "{id}");
            run_command(&mut app, &format!("/model {id}"));
            let screen = draw(&mut app, 240, 20);
            let expected = format!(
                "ctx 48k/{}k plan ({}%) · max {}k",
                model.context_window / 1000,
                48_000 * 100 / model.context_window,
                max / 1000
            );
            assert!(screen.contains(&expected), "{id}: {screen}");
        }

        let sonnet = row("claude-sonnet-5");
        assert!(sonnet.max_context_window <= Some(sonnet.context_window));
        run_command(&mut app, "/model claude-sonnet-5");
        let screen = draw(&mut app, 240, 20);
        let expected = format!(
            "ctx 48k/{}k ({}%)",
            sonnet.context_window / 1000,
            48_000 * 100 / sonnet.context_window
        );
        assert!(screen.contains(&expected), "{screen}");
        assert!(!screen.contains(" · max "), "{screen}");
    }

    #[test]
    fn footer_keeps_context_compact_without_a_larger_maximum() {
        let mut app = test_app();
        let mut model = app.agent.state().model;
        model.context_window = 272_000;
        app.context_tokens = 48_000;

        for maximum in [None, Some(272_000)] {
            model.max_context_window = maximum;
            app.agent.set_model(model.clone());
            let screen = draw(&mut app, 240, 20);
            assert!(screen.contains("ctx 48k/272k (17%)"), "{screen}");
            assert!(!screen.contains(" · max "), "{screen}");
        }
    }

    #[test]
    fn footer_marks_only_discovered_ollama_windows_as_assumed() {
        let mut app = test_app();
        let model = cupel_coding_agent::ollama::models_from_tags(
            &serde_json::json!({"models": [{"name": "qwen3:8b"}]}),
            "http://localhost:11434",
        )
        .remove(0);
        app.meta.models.push(model.clone());
        app.context_tokens = 2_000;

        run_command(&mut app, "/model qwen3:8b");
        let screen = draw(&mut app, 240, 20);
        assert!(screen.contains("ctx 2k/4k assumed (48%)"), "{screen}");
        assert!(!screen.contains(" · max "), "{screen}");

        // Explicit models.json rows carry no discovery marker, even when
        // the provider and context window are identical to the fallback.
        let mut configured = serde_json::to_value(model).unwrap();
        configured["compat"]
            .as_object_mut()
            .unwrap()
            .remove("contextWindowAssumed");
        let configured = serde_json::from_value(configured).unwrap();
        app.meta.models = cupel_coding_agent::models::merge_models(vec![
            app.meta.models.clone(),
            vec![configured],
        ]);
        run_command(&mut app, "/model qwen3:8b");
        let screen = draw(&mut app, 240, 20);
        assert!(screen.contains("ctx 2k/4k (48%)"), "{screen}");
    }

    #[tokio::test]
    async fn a_length_stop_is_flagged_and_never_promoted_to_an_answer() {
        let mut app = test_app();
        app.on_agent_event(Some(AgentEvent::MessageUpdate {
            event: cupel_core::types::AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: "half an ans".into(),
            },
        }))
        .await;
        app.on_agent_event(Some(AgentEvent::MessageEnd {
            message: assistant_message("half an ans", cupel_core::types::StopReason::Length, 0),
        }))
        .await;
        app.on_agent_event(Some(AgentEvent::AgentEnd {
            messages: Vec::new(),
        }))
        .await;
        let screen = draw(&mut app, 100, 20);
        assert!(screen.contains("error: response was truncated"), "{screen}");
        assert!(
            matches!(&app.transcript.cells[0], Cell::Assistant { .. }),
            "a fragment is not an answer"
        );
    }

    #[tokio::test]
    async fn compaction_summary_becomes_its_own_cell() {
        let mut app = test_app();
        app.on_agent_event(Some(AgentEvent::CompactionEnd {
            tokens_before: 48_000,
            tokens_after: 12_000,
            error: None,
            summary: Some("## Goal\nfix the retry backoff".into()),
            messages: Vec::new(),
        }))
        .await;
        assert!(matches!(
            app.transcript.cells.last(),
            Some(Cell::Summary { .. })
        ));
        let screen = draw(&mut app, 100, 20);
        assert!(screen.contains("[context summary]"), "{screen}");
        assert!(screen.contains("fix the retry backoff"), "{screen}");
        assert_eq!(
            style_of(&mut app, "fix the retry backoff").fg,
            Some(Color::White)
        );
    }

    #[test]
    fn a_resumed_checkpoint_is_a_summary_cell_not_a_user_prompt() {
        let model = cupel_core::catalog::builtin_models().remove(0);
        let registry = Arc::new(cupel_core::provider::Registry::new());
        let mut options = AgentOptions::new(model, registry);
        options.messages = vec![cupel_agent::AgentMessage::user_text(format!(
            "{}\n\n## Goal\nship it",
            cupel_agent::compaction::COMPACTION_MARKER
        ))];
        let recorder = cupel_coding_agent::session::SessionRecorder::new(
            None,
            std::path::Path::new("/tmp"),
            "cupel-resumed",
            "test-model",
        );
        let mut app = App::new(
            Agent::new(options),
            SessionMeta {
                model_name: "Test Model".into(),
                provider: "test".into(),
                cwd: "/tmp".into(),
                templates: Vec::new(),
                models: cupel_core::catalog::builtin_models(),
                settings: cupel_coding_agent::settings::Settings::default(),
                home: None,
                startup_warning: None,
                warnings: Vec::new(),
                context_files: Vec::new(),
                base_system_prompt: String::new(),
            },
            recorder,
        );
        assert!(
            matches!(&app.transcript.cells[1], Cell::Summary { text } if text.starts_with("## Goal"))
        );
        let screen = draw(&mut app, 100, 20);
        assert!(screen.contains("[context summary]"), "{screen}");
        assert!(!screen.contains("> [Conversation summary"), "{screen}");
    }

    /// The User cells in transcript order: which prompts reached a run.
    fn user_prompts(app: &App) -> Vec<&str> {
        app.transcript
            .cells
            .iter()
            .filter_map(|cell| match cell {
                Cell::User { text } => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn enter_while_running_queues_the_prompt() {
        let mut app = test_app();
        app.start_run("build it");
        submit_paste(&mut app, "no, the other file");
        assert!(app.input.is_empty(), "the prompt left the input box");
        assert_eq!(app.queued, ["no, the other file"]);
        let screen = draw(&mut app, 80, 20);
        assert!(screen.contains("Follow-up: no, the other file"), "{screen}");
        assert!(screen.contains("alt+up to edit all queued messages"));

        // The empty registry fails every run at once. The queued prompt
        // outlives the failed run and starts the next one.
        while app.is_running() {
            let event = app.next_event().await;
            app.on_event(event).await;
        }
        assert_eq!(user_prompts(&app), ["build it", "no, the other file"]);
        assert!(app.queued.is_empty());
        assert!(!draw(&mut app, 80, 20).contains("Follow-up:"));
    }

    #[tokio::test]
    async fn alt_up_and_esc_take_queued_prompts_back() {
        let mut app = test_app();
        app.start_run("build it");
        submit_paste(&mut app, "first");

        // Alt+Up: back into the input box, the run keeps going.
        app.on_terminal_event(Event::Key(KeyEvent::new(KeyCode::Up, KeyModifiers::ALT)));
        assert_eq!(app.input.text(), "first");
        assert!(app.queued.is_empty());
        assert!(app.agent.cancel_token().is_some_and(|t| !t.is_cancelled()));

        // Queue it again plus one more, start a draft, then Esc: all of it
        // lands in the input box, oldest first, and the run is aborted.
        app.on_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
        submit_paste(&mut app, "second");
        type_text(&mut app, "draft");
        app.on_terminal_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
        assert_eq!(app.input.text(), "first\n\nsecond\n\ndraft");
        assert!(app.agent.cancel_token().is_some_and(|t| t.is_cancelled()));
        // Taken back means never sent: the agent's copy is gone as well.
        assert!(app.agent.take_follow_up().is_none());

        while app.is_running() {
            let event = app.next_event().await;
            app.on_event(event).await;
        }
        assert_eq!(user_prompts(&app), ["build it"]);
    }

    #[tokio::test]
    async fn a_delivered_prompt_closes_the_turn_before_it() {
        let mut app = test_app();
        app.queued.push("next".to_string());
        app.on_agent_event(Some(AgentEvent::MessageUpdate {
            event: cupel_core::types::AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: "first answer".into(),
            },
        }))
        .await;
        app.on_agent_event(Some(AgentEvent::MessageEnd {
            message: cupel_agent::AgentMessage::user_text("next"),
        }))
        .await;
        assert!(app.queued.is_empty(), "delivered, so no longer waiting");
        assert!(matches!(
            app.transcript.cells.as_slice(),
            [Cell::Answer { text: answer }, Cell::User { text: prompt }]
                if answer == "first answer" && prompt == "next"
        ));
    }

    #[tokio::test]
    async fn ctrl_enter_while_running_steers_the_prompt() {
        let mut app = test_app();
        app.start_run("build it");
        type_text(&mut app, "use the other file");
        app.on_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::CONTROL,
        )));
        assert!(app.input.is_empty(), "the prompt left the input box");
        assert_eq!(app.steering, ["use the other file"]);
        assert!(app.queued.is_empty(), "steered, not queued");
        // Ctrl+J is the fallback for terminals without keyboard enhancement.
        type_text(&mut app, "and keep the tests");
        app.on_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::Char('j'),
            KeyModifiers::CONTROL,
        )));
        assert_eq!(app.steering, ["use the other file", "and keep the tests"]);
        let screen = draw(&mut app, 80, 20);
        assert!(screen.contains("Steer: use the other file"), "{screen}");
        assert!(screen.contains("Steer: and keep the tests"), "{screen}");

        // The empty registry fails every run at once. Both steered prompts
        // reach the model together, as one block.
        while app.is_running() {
            let event = app.next_event().await;
            app.on_event(event).await;
        }
        assert_eq!(
            user_prompts(&app),
            ["build it", "use the other file", "and keep the tests"]
        );
        assert!(app.steering.is_empty());
        assert!(!draw(&mut app, 80, 20).contains("Steer:"));
    }

    #[tokio::test]
    async fn a_prompt_steered_after_the_last_look_starts_the_next_run() {
        let mut app = test_app();
        app.start_run("build it");
        let mut steered = false;
        while app.is_running() {
            let event = app.next_event().await;
            // The run is over but its end is not handled yet: the loop
            // will never look at the steering queue again.
            if !steered
                && matches!(
                    event,
                    crate::app::AppEvent::Agent(Some(AgentEvent::AgentEnd { .. }))
                )
            {
                type_text(&mut app, "late");
                app.on_terminal_event(Event::Key(KeyEvent::new(
                    KeyCode::Enter,
                    KeyModifiers::CONTROL,
                )));
                steered = true;
            }
            app.on_event(event).await;
        }
        assert!(steered);
        assert_eq!(user_prompts(&app), ["build it", "late"]);
        assert!(app.steering.is_empty());
    }

    #[tokio::test]
    async fn a_reply_cut_short_by_steering_is_no_error() {
        let mut app = test_app();
        app.steering.push("smaller steps".to_string());
        app.on_agent_event(Some(AgentEvent::MessageUpdate {
            event: cupel_core::types::AssistantMessageEvent::TextDelta {
                content_index: 0,
                delta: "half a plan".into(),
            },
        }))
        .await;
        app.on_agent_event(Some(AgentEvent::MessageEnd {
            message: assistant_message("half a plan", cupel_core::types::StopReason::Aborted, 0),
        }))
        .await;
        app.on_agent_event(Some(AgentEvent::MessageEnd {
            message: cupel_agent::AgentMessage::user_text("smaller steps"),
        }))
        .await;
        assert!(app.steering.is_empty(), "delivered, so no longer waiting");
        // The partial prose stays plain prose: it is no final answer.
        assert!(matches!(
            app.transcript.cells.as_slice(),
            [
                Cell::Assistant { text: partial },
                Cell::Notice { text: notice },
                Cell::User { text: prompt },
            ] if partial == "half a plan"
                && notice == "interrupted to steer"
                && prompt == "smaller steps"
        ));
    }

    #[tokio::test]
    async fn esc_takes_steered_prompts_back_first() {
        let mut app = test_app();
        app.start_run("build it");
        type_text(&mut app, "steered");
        app.on_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::CONTROL,
        )));
        submit_paste(&mut app, "queued");
        type_text(&mut app, "draft");
        app.on_terminal_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
        assert_eq!(app.input.text(), "steered\n\nqueued\n\ndraft");
        assert!(app.steering.is_empty() && app.queued.is_empty());
        // Taken back means never sent: the agent's copies are gone as well.
        assert!(app.agent.take_steering().is_empty());
        assert!(app.agent.take_follow_up().is_none());
    }

    #[tokio::test]
    async fn the_spinner_turns_with_every_tick_while_running() {
        let mut app = test_app();
        // A run against the empty registry errors in the background; until
        // its events are pumped the app counts as running.
        app.start_run("spin");
        assert!(draw(&mut app, 80, 20).contains("◓ waiting for the model · 0s"));
        assert_eq!(style_of(&mut app, "─").fg, CUPEL.input_border_busy.fg);
        app.tick();
        assert!(draw(&mut app, 80, 20).contains("◑ waiting for the model · 0s"));
        while app.is_running() {
            let event = app.next_event().await;
            app.on_event(event).await;
        }
        assert!(!draw(&mut app, 80, 20).contains(" prompt "));
        assert_eq!(style_of(&mut app, "─").fg, CUPEL.input_border_idle.fg);
    }

    #[tokio::test]
    async fn the_title_follows_what_the_agent_does() {
        use cupel_core::types::AssistantMessageEvent;
        let mut app = test_app();
        app.start_run("build it");
        // Feed one event, then return the drawn screen.
        async fn after(app: &mut App, event: AgentEvent) -> String {
            app.on_agent_event(Some(event)).await;
            draw(app, 100, 20)
        }
        let thinking = |delta: &str| AgentEvent::MessageUpdate {
            event: AssistantMessageEvent::ThinkingDelta {
                content_index: 0,
                delta: delta.to_string(),
            },
        };

        let screen = after(&mut app, thinking("let me look")).await;
        assert!(screen.contains("◓ thinking · 0s"), "{screen}");
        let screen = after(
            &mut app,
            thinking("\n\n**Reading the parser**\n\nIt is long."),
        )
        .await;
        assert!(screen.contains("◓ Reading the parser · 0s"), "{screen}");
        let text = AgentEvent::MessageUpdate {
            event: AssistantMessageEvent::TextDelta {
                content_index: 1,
                delta: "Found it.".into(),
            },
        };
        assert!(after(&mut app, text).await.contains("◓ writing · 0s"));
        let call = AgentEvent::MessageUpdate {
            event: AssistantMessageEvent::ToolCallStart { content_index: 2 },
        };
        assert!(
            after(&mut app, call)
                .await
                .contains("◓ writing a tool call · 0s")
        );
        let run = AgentEvent::ToolExecutionStart {
            tool_call_id: "call_1".into(),
            tool_name: "bash".into(),
        };
        assert!(after(&mut app, run).await.contains("◓ running bash · 0s"));
        let screen = after(&mut app, AgentEvent::TurnStart).await;
        assert!(screen.contains("◓ waiting for the model · 0s"), "{screen}");
    }

    #[tokio::test]
    async fn a_finished_run_says_how_long_it_worked() {
        let mut app = test_app();
        app.start_run("build it");
        while app.is_running() {
            let event = app.next_event().await;
            app.on_event(event).await;
        }
        assert!(matches!(
            app.transcript.cells.last(),
            Some(Cell::Usage { text }) if text == "worked for 0s"
        ));
        assert!(app.run_started.is_none(), "the clock stops with the run");
    }

    #[test]
    fn the_reasoning_headline_is_the_last_bold_line() {
        use crate::app::reasoning_headline;
        let summary = "**Reading the parser**\n\nIt is long.\n\n**Fixing the bug**\n\nNow.";
        assert_eq!(reasoning_headline(summary), Some("Fixing the bug"));
        assert_eq!(reasoning_headline("plain thinking, **bold** inside"), None);
        assert_eq!(reasoning_headline("****"), None);
    }

    #[test]
    fn elapsed_time_reads_like_a_clock() {
        use std::time::Duration;
        assert_eq!(format_elapsed(Duration::from_secs(42)), "42s");
        assert_eq!(format_elapsed(Duration::from_secs(65)), "1m 05s");
    }

    #[tokio::test]
    async fn a_tool_call_renders_as_its_own_header_line() {
        use cupel_agent::AgentTool;
        // An agent that has the bash tool: the header comes from the tool.
        let model = cupel_core::catalog::builtin_models().remove(0);
        let registry = Arc::new(cupel_core::provider::Registry::new());
        let mut options = AgentOptions::new(model, registry);
        options.tools = vec![
            Arc::new(cupel_coding_agent::tools::bash::BashTool::new("/tmp")) as Arc<dyn AgentTool>,
        ];
        let recorder = cupel_coding_agent::session::SessionRecorder::new(
            None,
            std::path::Path::new("/tmp"),
            "cupel-test",
            "test-model",
        );
        let mut app = App::new(
            Agent::new(options),
            SessionMeta {
                model_name: "Test Model".into(),
                provider: "test".into(),
                cwd: "/tmp".into(),
                templates: Vec::new(),
                models: cupel_core::catalog::builtin_models(),
                settings: cupel_coding_agent::settings::Settings::default(),
                home: None,
                startup_warning: None,
                warnings: Vec::new(),
                context_files: Vec::new(),
                base_system_prompt: String::new(),
            },
            recorder,
        );
        let tool_call = |name: &str, arguments: serde_json::Value| AgentEvent::MessageUpdate {
            event: cupel_core::types::AssistantMessageEvent::ToolCallEnd {
                content_index: 0,
                tool_call: cupel_core::types::ToolCall {
                    id: format!("call_{name}"),
                    name: name.into(),
                    arguments,
                },
            },
        };
        app.on_agent_event(Some(tool_call(
            "bash",
            serde_json::json!({"command": "cargo test -p cupel-core"}),
        )))
        .await;
        // A tool this agent does not have falls back to the bare name.
        app.on_agent_event(Some(tool_call("magic", serde_json::json!({"x": 1}))))
            .await;

        let screen = draw(&mut app, 80, 24);
        assert!(screen.contains("$ cargo test -p cupel-core"), "{screen}");
        assert!(!screen.contains("[bash]"), "no raw JSON header:\n{screen}");
        assert!(!screen.contains("\"command\""), "{screen}");
        assert!(screen.contains("magic"), "{screen}");
        assert!(!screen.contains("{\"x\":1}"), "{screen}");
    }

    #[tokio::test]
    async fn a_patch_result_renders_as_a_colored_diff() {
        let mut app = test_app();
        app.transcript.cells.push(Cell::Tool {
            id: "call_1".into(),
            name: "apply_patch".into(),
            call: "apply_patch M a.rs".into(),
            expanded: false,
            started_at: None,
            live: None,
            result: None,
        });
        app.on_agent_event(Some(AgentEvent::ToolExecutionEnd {
            tool_call_id: "call_1".into(),
            tool_name: "apply_patch".into(),
            result: cupel_agent::types::AgentToolResult {
                content: vec![cupel_core::types::ToolResultContent::Text(
                    cupel_core::types::TextContent::plain(
                        "Success. Updated the following files:\nM a.rs",
                    ),
                )],
                details: Some(
                    serde_json::json!({"diff": " 1 fn a() {\n-2     old();\n+2     new();\n 3 }"}),
                ),
                terminate: false,
            },
            is_error: false,
        }))
        .await;

        let screen = draw(&mut app, 80, 24);
        assert!(screen.contains("-2     old();"), "{screen}");
        assert!(screen.contains("+2     new();"), "{screen}");
        assert!(!screen.contains("Updated the following"), "{screen}");
        assert_eq!(style_of(&mut app, "+2     new();").fg, Some(Color::Green));
        assert_eq!(style_of(&mut app, "-2     old();").fg, Some(Color::Red));
        assert_eq!(style_of(&mut app, "1 fn a() {").fg, Some(Color::DarkGray));
    }

    #[test]
    fn ctrl_y_toggles_selection_mode_and_updates_the_footer() {
        let mut app = test_app();
        assert!(app.mouse_captured);
        let screen = draw(&mut app, 200, 20);
        assert!(screen.contains("ctrl+o copy"), "hint missing:\n{screen}");
        assert!(screen.contains("ctrl+y select"), "hint missing:\n{screen}");

        // Ctrl+Y only requests the toggle (the event loop owns the
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
        // Bracketed paste delivers the whole clipboard as one event; the
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
        // the model list no extra keystroke needed.
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
        assert_eq!(app.input.text(), "/model claude-haiku-5-5 ");
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

    /// Type a command, close the popup typing opened, and submit it.
    pub(crate) fn run_command(app: &mut App, command: &str) {
        type_text(app, command);
        app.on_terminal_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
        app.on_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
    }

    pub(crate) fn has_notice(app: &App, needle: &str) -> bool {
        app.transcript
            .cells
            .iter()
            .any(|c| matches!(c, Cell::Notice { text } if text.contains(needle)))
    }

    /// test_app with two presets and a known base prompt. Rebuilt through
    /// App::new, because App::new turns the presets into completions.
    fn test_app_with_presets() -> App {
        let mut app = test_app();
        app.meta.settings = serde_json::from_str(
            r#"{"model": {
                "fast": {"provider": "anthropic", "model": "claude-haiku-5-5",
                         "thinkingLevel": "low", "prompt": "Answer in one sentence."},
                "deep": {"provider": "anthropic", "model": "claude-sonnet-5",
                         "thinkingLevel": "xhigh"}
            }}"#,
        )
        .unwrap();
        app.meta.base_system_prompt = "BASE".into();
        App::new(app.agent, app.meta, app.recorder)
    }

    #[test]
    fn preset_command_completes_lists_and_switches_everything_at_once() {
        use cupel_core::types::ThinkingLevel;

        // `/preset ` offers the preset names from settings.json.
        let mut app = test_app_with_presets();
        type_text(&mut app, "/preset ");
        let (rows, _) = app.autocomplete.visible().expect("preset rows");
        let values: Vec<&str> = rows.iter().map(|r| r.value.as_str()).collect();
        assert_eq!(values, ["deep", "fast"]);

        // Bare /preset lists what each preset switches to.
        let mut app = test_app_with_presets();
        run_command(&mut app, "/preset");
        assert!(has_notice(
            &app,
            "fast  - anthropic/claude-haiku-5-5, thinking low, + prompt"
        ));
        assert!(has_notice(
            &app,
            "deep  - anthropic/claude-sonnet-5, thinking xhigh"
        ));

        // One command switches model, thinking level, and prompt.
        run_command(&mut app, "/preset fast");
        let state = app.agent.state();
        assert_eq!(state.model.id, "claude-haiku-5-5");
        assert_eq!(state.thinking_level, Some(ThinkingLevel::Low));
        assert_eq!(state.system_prompt, "BASE\n\nAnswer in one sentence.");
        assert_eq!(app.meta.provider, "anthropic");
        assert!(has_notice(&app, "preset fast activated"));

        // A preset without a prompt replaces the old prompt, never stacks.
        run_command(&mut app, "/preset deep");
        let state = app.agent.state();
        assert_eq!(state.model.id, "claude-sonnet-5");
        assert_eq!(state.thinking_level, Some(ThinkingLevel::XHigh));
        assert_eq!(state.system_prompt, "BASE");

        // Unknown names and unknown models change nothing.
        run_command(&mut app, "/preset nope");
        assert!(has_notice(&app, "unknown preset: nope"));
        app.meta.settings.presets.insert(
            "broken".into(),
            cupel_coding_agent::settings::Preset {
                provider: "anthropic".into(),
                model: "claude-nope".into(),
                thinking_level: cupel_coding_agent::settings::ThinkingSetting::Max,
                prompt: None,
            },
        );
        run_command(&mut app, "/preset broken");
        let state = app.agent.state();
        assert_eq!(state.model.id, "claude-sonnet-5", "model unchanged");
        assert_eq!(state.thinking_level, Some(ThinkingLevel::XHigh));

        // Without presets, /preset says where they belong.
        let mut app = test_app();
        run_command(&mut app, "/preset");
        assert!(has_notice(&app, "no presets defined"));
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
        let saved = cupel_coding_agent::settings::load_settings(&path).unwrap();
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

    /// A key-requiring model on a provider id with no env-var mapping
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
        // test does not mind the acceptance is the point.
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
        use crate::app::ReloadTarget;
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
    async fn every_reload_target_shows_configuration_warnings() {
        use crate::app::ReloadTarget;

        for (name, target) in [
            ("current", ReloadTarget::Current),
            ("new", ReloadTarget::New),
            ("resume", ReloadTarget::Resume("cupel-old".into())),
        ] {
            let root = std::env::temp_dir().join(format!("cupel-ui-reload-warnings-{name}"));
            let _ = std::fs::remove_dir_all(&root);
            let mut app = test_app_with_home(&root, "cupel-current");
            // A real registry avoids unrelated warnings for built-in models.
            app.agent = Agent::new(AgentOptions::new(
                acme_model(),
                Arc::new(cupel_core::default_registry()),
            ));
            std::fs::create_dir_all(root.join("proj/.cupel")).unwrap();
            std::fs::write(root.join("home/settings.json"), "{broken").unwrap();
            std::fs::write(root.join("home/models.json"), "{broken").unwrap();
            std::fs::write(root.join("proj/.cupel/bash-deny"), "[unclosed").unwrap();
            std::fs::write(
                root.join("proj/.cupel/settings.json"),
                r#"{"providers":{"acme":"project-secret"}}"#,
            )
            .unwrap();
            let dir = app.recorder.sessions_dir().unwrap();
            std::fs::create_dir_all(dir).unwrap();
            std::fs::write(
                dir.join("cupel-old.jsonl"),
                format!(
                    "{}\n",
                    serde_json::json!({
                        "version": 1, "sessionId": "cupel-old", "cwd": "x",
                        "model": "acme-1", "startedAt": 1000
                    })
                ),
            )
            .unwrap();

            let mut app = app.hot_reload(target).await;
            assert_eq!(app.agent.api_key(), None, "project keys are never honored");
            for needle in [
                "ignoring settings file",
                "ignoring models file",
                "invalid bash-deny pattern",
                "API keys belong",
            ] {
                assert_eq!(
                    app.transcript
                        .cells
                        .iter()
                        .filter(|cell| matches!(
                            cell, Cell::Notice { text } if text.contains(needle)
                        ))
                        .count(),
                    1,
                    "{name}: {needle}"
                );
            }
            if name == "current" {
                assert!(has_notice(&app, "reloaded in place with warnings"));
            }
            let screen = draw(&mut app, 160, 40);
            assert!(
                screen.contains("ignoring settings file"),
                "{name}: {screen}"
            );
            assert!(
                screen.contains("invalid bash-deny pattern"),
                "{name}: {screen}"
            );
            assert!(!screen.contains("project-secret"));
            assert!(
                app.agent.state().messages.is_empty(),
                "not sent to the model"
            );
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[tokio::test]
    async fn a_preset_prompt_survives_both_hot_reloads() {
        use crate::app::ReloadTarget;
        let root = std::env::temp_dir().join("cupel-ui-preset-reload");
        let _ = std::fs::remove_dir_all(&root);
        let mut app = test_app_with_home(&root, "cupel-current");
        std::fs::write(
            root.join("home/settings.json"),
            r#"{"model": {"fast": {"provider": "anthropic", "model": "claude-haiku-5-5",
                                   "thinkingLevel": "low", "prompt": "PRESET RULES"}}}"#,
        )
        .unwrap();
        app.meta.settings =
            cupel_coding_agent::settings::load_settings(&root.join("home/settings.json")).unwrap();
        run_command(&mut app, "/preset fast");
        // The test app's base prompt is empty: only the preset part is left.
        assert_eq!(app.agent.state().system_prompt, "\n\nPRESET RULES");

        // In place: the running system prompt is kept as it is.
        let app = app.hot_reload(ReloadTarget::Current).await;
        assert_eq!(app.agent.state().system_prompt, "\n\nPRESET RULES");

        // Resume: a fresh base prompt, the preset prompt still at its end.
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
        let app = app
            .hot_reload(ReloadTarget::Resume("cupel-old".into()))
            .await;
        let prompt = app.agent.state().system_prompt;
        assert!(prompt.starts_with("You are an expert coding assistant"));
        assert_eq!(
            prompt.strip_suffix("\n\nPRESET RULES"),
            Some(app.meta.base_system_prompt.as_str())
        );
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
        assert!(screen.contains("find the bug"), "{screen}");

        while app.is_running() {
            let event = app.next_event().await;
            app.on_event(event).await;
        }
        let path = app
            .recorder
            .sessions_dir()
            .expect("home is set")
            .join("cupel-task.jsonl");
        let (_, messages) = cupel_coding_agent::session::load_transcript(&path).unwrap();
        let recorded = matches!(
            messages.first(),
            Some(cupel_agent::AgentMessage::Llm(cupel_core::types::Message::User(user)))
                if matches!(&user.content,
                    cupel_core::types::UserContentBody::Text(t) if t == "find the bug")
        );
        assert!(recorded, "prompt missing from the transcript file");
        assert!(matches!(
            messages.get(1),
            Some(cupel_agent::AgentMessage::Llm(cupel_core::types::Message::Assistant(a)))
                if a.stop_reason == cupel_core::types::StopReason::Error
        ));
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
    fn model_status_shows_the_thinking_level_for_reasoning_models() {
        let mut app = test_app();
        // AgentOptions::new leaves thinking at None -> "thinking off".
        let screen = draw(&mut app, 120, 20);
        assert!(
            screen.lines().nth(14).unwrap().contains("thinking off"),
            "{screen}"
        );

        app.agent
            .set_thinking_level(Some(cupel_core::types::ThinkingLevel::Medium));
        let screen = draw(&mut app, 120, 20);
        assert!(
            screen.lines().nth(14).unwrap().contains("thinking medium"),
            "{screen}"
        );
        assert!(!screen.contains("thinking off"), "{screen}");
    }

    #[test]
    fn model_and_effort_are_right_aligned_above_the_prompt_not_in_the_footer() {
        let mut app = test_app();
        app.agent
            .set_thinking_level(Some(cupel_core::types::ThinkingLevel::High));
        let label = "Test Model (test) | thinking high ";
        for width in [80, 120, 200] {
            let screen = draw(&mut app, width, 20);
            let rows = screen.lines().collect::<Vec<_>>();
            assert_eq!(
                rows[14],
                format!("{}{label}", " ".repeat(usize::from(width) - label.len())),
                "{screen}"
            );
            assert_eq!(
                rows[15],
                "─".repeat(usize::from(width)),
                "model status has its own row above the prompt border"
            );
            assert!(rows[18].starts_with(" cupel-test | 0 in /"), "{screen}");
            assert!(
                !rows[18].contains("Test Model") && !rows[18].contains("thinking"),
                "{screen}"
            );
        }
    }

    #[test]
    fn model_status_omits_effort_for_non_reasoning_models() {
        let mut app = test_app();
        let mut model = app.agent.state().model;
        model.reasoning = false;
        app.agent.set_model(model);
        let screen = draw(&mut app, 120, 20);
        let row = screen.lines().nth(14).unwrap();
        assert!(row.ends_with("Test Model (test) "), "{screen}");
        assert!(!row.contains("thinking"), "{screen}");
    }

    #[test]
    fn the_transcript_is_one_borderless_column() {
        let mut app = test_app();
        app.transcript.cells.push(Cell::User {
            text: "task".into(),
        });
        app.transcript.append_thinking("let me look");
        app.transcript.cells.push(Cell::Tool {
            id: "1".into(),
            name: "read".into(),
            call: "read src/main.rs".into(),
            expanded: false,
            started_at: None,
            live: None,
            result: None,
        });
        app.transcript.append_assistant("found it");
        let screen = draw(&mut app, 80, 20);
        // One frame: no tools pane, no band rule.
        assert!(!screen.contains(" conversation "), "{screen}");
        assert!(!screen.contains(" tools "), "{screen}");
        assert!(!screen.contains("─ 1 "), "{screen}");
        // Cells follow each other top to bottom in event order...
        let row_of = |needle: &str| {
            screen
                .lines()
                .position(|row| row.contains(needle))
                .unwrap_or_else(|| panic!("{needle:?} missing:\n{screen}"))
        };
        assert!(row_of("task") < row_of("let me look"));
        assert!(row_of("let me look") < row_of("read src/main.rs"));
        assert!(row_of("read src/main.rs") < row_of("found it"));
        // Only the pane margin remains before the user message marker.
        let row = screen.lines().nth(row_of("task")).unwrap();
        assert!(row.starts_with(" >> task"), "{row:?}");
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
        // click handler uses the test then exercises the real geometry
        // math instead of hardcoding a row.
        let line = app
            .last_line_cells
            .iter()
            .position(|cell| *cell == Some(1))
            .expect("thinking line mapped");
        let row = app.last_transcript_inner.y + (line - app.last_top_line) as u16;
        app.on_terminal_event(Event::Mouse(MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: app.last_transcript_inner.x + 1,
            row,
            modifiers: KeyModifiers::NONE,
        }));
        assert_eq!(app.selected_cell, Some(1));

        // The selected block renders with the highlight background.
        let _ = draw(&mut app, 80, 24);
        let style = style_of(&mut app, "private reasoning");
        assert_eq!(style.bg, CUPEL.selected.bg);

        // Ctrl+O queues the raw text and confirms with a notice.
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
        // ...and accents combine with it instead of replacing it.
        let weight = style_of(&mut app, "weight");
        assert_eq!(weight.fg, Some(Color::Magenta));
        assert!(weight.add_modifier.contains(Modifier::BOLD));
    }

    /// Submit one line as a command (popup closed first, like a user
    /// pressing esc before enter).
    fn submit_command(app: &mut App, line: &str) {
        type_text(app, line);
        app.on_terminal_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
        app.on_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
    }

    fn last_notice(app: &App) -> String {
        app.transcript
            .cells
            .iter()
            .rev()
            .find_map(|cell| match cell {
                Cell::Notice { text } => Some(text.clone()),
                _ => None,
            })
            .expect("a notice cell")
    }

    #[test]
    fn login_command_validates_before_spawning_anything() {
        // Every path here must answer without starting a flow the
        // real flows bind port 1455 and open a browser.
        let mut app = test_app();
        submit_command(&mut app, "/login");
        assert!(
            last_notice(&app).contains("/login openai-codex"),
            "usage lists the provider"
        );
        assert!(app.login.is_none());

        submit_command(&mut app, "/login anthropic");
        assert!(last_notice(&app).contains("unknown login provider"));
        assert!(app.login.is_none());

        // A paste with no waiting flow is redirected to start one.
        submit_command(
            &mut app,
            "/login openai-codex http://localhost:1455/auth/callback?code=x",
        );
        assert!(last_notice(&app).contains("no login waiting"));
        assert!(app.login.is_none());
    }

    #[test]
    fn esc_cancels_a_waiting_login_before_touching_the_run() {
        let mut app = test_app();
        let (flow, _events, cancel, _code_rx) = crate::login::stub_flow();
        app.login = Some(flow);

        app.on_terminal_event(Event::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
        assert!(app.login.is_none(), "esc drops the flow");
        assert!(cancel.is_cancelled(), "dropping cancelled the task");
        assert!(last_notice(&app).contains("login cancelled"));
    }

    /// Type + enter without the popup-closing esc: while a login waits,
    /// esc (with no popup visible) would cancel exactly the flow this
    /// test needs alive. The paste token matches no candidate, so the
    /// popup never opens and enter submits directly.
    fn submit_paste(app: &mut App, line: &str) {
        type_text(app, line);
        app.on_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        )));
    }

    #[test]
    fn a_pasted_code_reaches_the_waiting_flow_once() {
        let mut app = test_app();
        let (flow, _events, _cancel, mut code_rx) = crate::login::stub_flow();
        app.login = Some(flow);

        submit_paste(&mut app, "/login openai-codex code-from-browser");
        assert!(
            last_notice(&app).contains("code submitted"),
            "{}",
            last_notice(&app)
        );
        assert_eq!(code_rx.try_recv().unwrap(), "code-from-browser");
        // The oneshot is spent; a second paste is told so.
        submit_paste(&mut app, "/login openai-codex another");
        assert!(last_notice(&app).contains("cannot take a pasted code"));
    }

    #[tokio::test]
    async fn login_events_flow_into_notices_and_done_clears_the_flow() {
        let mut app = test_app();
        let (flow, events, _cancel, _code_rx) = crate::login::stub_flow();
        app.login = Some(flow);

        events
            .send(crate::login::LoginEvent::Notice(
                "open the browser".to_string(),
            ))
            .unwrap();
        let event = app.next_event().await;
        app.on_event(event).await;
        assert!(last_notice(&app).contains("open the browser"));
        assert!(app.login.is_some(), "notices keep the flow alive");

        events
            .send(crate::login::LoginEvent::Done(Ok(
                "logged in with ChatGPT (account acc-1)".to_string(),
            )))
            .unwrap();
        let event = app.next_event().await;
        app.on_event(event).await;
        assert!(app.login.is_none(), "done ends the flow");
        let notice = last_notice(&app);
        assert!(notice.contains("logged in with ChatGPT"), "{notice}");
        assert!(notice.contains("/provider openai-codex"), "{notice}");
        assert!(notice.contains("default codex/gpt-6.1-sol"), "{notice}");
    }

    #[test]
    fn logout_lists_and_removes_stored_credentials() {
        let root = std::env::temp_dir().join("cupel-ui-logout");
        let _ = std::fs::remove_dir_all(&root);
        let mut app = test_app_with_home(&root, "cupel-logout-test");
        let home = root.join("home");

        submit_command(&mut app, "/logout");
        assert!(last_notice(&app).contains("no stored logins"));

        let credential = cupel_core::oauth::openai_codex::OAuthCredential {
            access: "a".into(),
            refresh: "r".into(),
            expires: 1,
            account_id: "acc".into(),
        };
        cupel_coding_agent::auth::save_credential(Some(&home), "openai-codex", &credential)
            .unwrap();

        submit_command(&mut app, "/logout");
        assert!(last_notice(&app).contains("openai-codex"));
        submit_command(&mut app, "/logout openai-codex");
        assert!(last_notice(&app).contains("logged out"));
        assert!(!cupel_coding_agent::auth::has_credential(
            Some(&home),
            "openai-codex"
        ));
        // A second logout reports the absence instead of erroring.
        submit_command(&mut app, "/logout openai-codex");
        assert!(last_notice(&app).contains("no stored login for openai-codex"));
    }

    #[test]
    fn provider_listing_shows_the_codex_login_status() {
        let mut app = test_app(); // home: None -> never logged in
        submit_command(&mut app, "/provider");
        let notice = last_notice(&app);
        assert!(
            notice.contains(
                "openai-codex  - default codex/gpt-6.1-sol, not logged in - /login openai-codex"
            ),
            "{notice}"
        );
        submit_command(&mut app, "/provider openai-codex");
        assert_eq!(app.meta.provider, "openai-codex");
        assert_eq!(app.meta.model_name, "GPT-6.1 Sol");
        // Not the last notice: the test app runs with thinking off, which
        // GPT-6.1 Sol cannot do, so the thinking notice follows this one.
        assert!(has_notice(&app, "model codex/gpt-6.1-sol;"));
        // And the key path is closed: codex takes logins, not keys.
        submit_command(&mut app, "/provider openai-codex sk-pasted");
        let notice = app
            .transcript
            .cells
            .iter()
            .rev()
            .filter_map(|cell| match cell {
                Cell::Notice { text } => Some(text.clone()),
                _ => None,
            })
            .find(|text| text.contains("does not take an API key"))
            .expect("key refusal notice");
        assert!(notice.contains("openai-codex"), "{notice}");
    }

    #[tokio::test]
    async fn program_status_tracks_work_question_resume_and_success() {
        use crate::app::Activity;
        use cupel_coding_agent::program_status::{BlockedKind, State};
        let mut app = test_app();
        assert_eq!(app.program_status(), (State::Idle, None));
        let (events, _sink) = cupel_agent::agent_loop::agent_event_channel();
        app.run_events = Some(events);
        assert_eq!(app.program_status().0, State::Working);
        let answer = open_question(&mut app);
        assert_eq!(
            app.program_status().0,
            State::Blocked(BlockedKind::Question)
        );
        assert_eq!(
            app.program_status().1.as_deref(),
            Some("Where should presets live?")
        );
        press(&mut app, KeyCode::Enter);
        assert_eq!(answer.await.unwrap(), Answer::Picked(1));
        assert_eq!(app.program_status().0, State::Working);
        for activity in [
            Activity::Retrying,
            Activity::Compacting,
            Activity::Running("bash".into()),
        ] {
            app.activity = activity;
            assert_eq!(app.program_status().0, State::Working);
        }
        app.on_agent_event(Some(AgentEvent::MessageEnd {
            message: assistant_message("ok", cupel_core::types::StopReason::Stop, 1),
        }))
        .await;
        assert_eq!(
            app.program_status().0,
            State::Working,
            "not done until AgentEnd"
        );
        app.on_agent_event(Some(AgentEvent::AgentEnd {
            messages: Vec::new(),
        }))
        .await;
        assert_eq!(app.program_status(), (State::Done, None));
    }

    #[tokio::test]
    async fn program_status_cancellation_is_not_failure_even_with_late_events() {
        use cupel_coding_agent::program_status::State;
        for reason in [
            cupel_core::types::StopReason::Aborted,
            cupel_core::types::StopReason::Error,
        ] {
            let mut app = test_app();
            let (events, _sink) = cupel_agent::agent_loop::agent_event_channel();
            app.run_events = Some(events);
            let _answer = open_question(&mut app);
            press(&mut app, KeyCode::Esc);
            assert_eq!(app.program_status(), (State::Idle, None));
            app.on_agent_event(Some(AgentEvent::MessageEnd {
                message: assistant_message("", reason, 1),
            }))
            .await;
            app.on_agent_event(Some(AgentEvent::AgentEnd {
                messages: Vec::new(),
            }))
            .await;
            assert_eq!(app.program_status(), (State::Idle, None));
            app.start_run("again");
            assert_eq!(app.program_status().0, State::Working);
            while app.is_running() {
                let event = app.next_event().await;
                app.on_event(event).await;
            }
            assert_eq!(app.program_status().0, State::Error);
        }
    }

    #[tokio::test]
    async fn program_status_login_wait_and_completion_restore_actual_state() {
        use crate::login::{LoginEvent, LoginState};
        use cupel_coding_agent::program_status::{BlockedKind, State};
        let mut app = test_app();
        let (flow, _events, _cancel, _code) = crate::login::stub_flow();
        app.login = Some(flow);
        assert_eq!(app.program_status().0, State::Working);
        app.on_login_event(Some(LoginEvent::Progress(
            LoginState::Waiting,
            "open browser".into(),
        )));
        assert_eq!(app.program_status().0, State::Blocked(BlockedKind::Auth));
        app.on_login_event(Some(LoginEvent::Progress(
            LoginState::Working,
            "exchanging".into(),
        )));
        assert_eq!(app.program_status().0, State::Working);
        app.on_login_event(Some(LoginEvent::Done(Ok("logged in".into()))));
        assert_eq!(app.program_status().0, State::Idle);

        let (events, _sink) = cupel_agent::agent_loop::agent_event_channel();
        app.run_events = Some(events);
        let (flow, _events, _cancel, _code) = crate::login::stub_flow();
        app.login = Some(flow);
        app.on_login_event(Some(LoginEvent::Progress(
            LoginState::Waiting,
            "enter code".into(),
        )));
        assert_eq!(app.program_status().0, State::Blocked(BlockedKind::Auth));
        app.on_login_event(Some(LoginEvent::Done(Ok("logged in".into()))));
        assert_eq!(app.program_status().0, State::Working);

        let (flow, _events, _cancel, _code) = crate::login::stub_flow();
        app.login = Some(flow);
        app.on_login_event(Some(LoginEvent::Progress(
            LoginState::Waiting,
            "enter code".into(),
        )));
        press(&mut app, KeyCode::Esc);
        assert_eq!(
            app.program_status().0,
            State::Working,
            "login cancellation resumes the run"
        );
    }

    #[tokio::test]
    async fn program_status_unexpected_stream_close_is_failure() {
        use cupel_coding_agent::program_status::State;
        let mut app = test_app();
        app.last_answer = Status::Done;
        let (events, _sink) = cupel_agent::agent_loop::agent_event_channel();
        app.run_events = Some(events);
        app.on_agent_event(None).await;
        assert_eq!(app.program_status().0, State::Error);
    }

    #[tokio::test]
    async fn status_follows_the_run_and_how_the_answer_ended() {
        use crate::app::Status;
        let mut app = test_app();
        assert_eq!(app.status(), Status::New);
        // A run against the empty registry fails in the background; until
        // its events are pumped, the session counts as running.
        app.start_run("hello");
        assert_eq!(app.status(), Status::Running);
        while app.is_running() {
            let event = app.next_event().await;
            app.on_event(event).await;
        }
        assert_eq!(app.status(), Status::Failed);
        // A good answer changes the mark back.
        app.on_agent_event(Some(AgentEvent::MessageEnd {
            message: assistant_message("ok", cupel_core::types::StopReason::Stop, 1),
        }))
        .await;
        assert_eq!(app.status(), Status::Done);
    }

    #[test]
    fn spinoff_commands_wait_for_the_sessions() {
        let mut app = test_app();
        run_command(&mut app, "/spinoff auth fast");
        let expected = cupel_coding_agent::commands::SpinoffCommand::Create {
            name: "auth".parse().unwrap(),
            preset: Some("fast".to_string()),
        };
        assert_eq!(app.pending_spinoff, Some(expected));
        // A bad name is refused at once, as a notice.
        let mut app = test_app();
        run_command(&mut app, "/spinoff Auth");
        assert!(app.pending_spinoff.is_none());
        assert!(has_notice(&app, "invalid spinoff name \"Auth\""));
    }

    #[test]
    fn ctrl_n_and_ctrl_p_switch_sessions_and_wrap_around() {
        use crate::sessions::{Session, Sessions};
        let mut sessions = Sessions::new(test_app());
        for label in ["auth", "docs"] {
            let app = test_app();
            let label = label.to_string();
            sessions.list.push(Session { label, app });
        }
        let ctrl = |c| Event::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL));

        sessions.on_terminal_event(ctrl('n'));
        assert_eq!(sessions.active, 1);
        sessions.on_terminal_event(ctrl('n'));
        sessions.on_terminal_event(ctrl('n'));
        assert_eq!(sessions.active, 0, "after the last comes the first");
        sessions.on_terminal_event(ctrl('p'));
        assert_eq!(sessions.active, 2, "before the first comes the last");
        // Every other key goes to the session on screen.
        let x = Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        sessions.on_terminal_event(x);
        assert_eq!(sessions.list[2].app.input.text(), "x");
        assert_eq!(sessions.list[0].app.input.text(), "");
        // The mouse state follows the terminal, not the session.
        sessions.active_mut().mouse_captured = false;
        sessions.on_terminal_event(ctrl('n'));
        assert!(!sessions.active().mouse_captured);
    }

    #[tokio::test]
    async fn events_arrive_from_sessions_that_are_not_on_screen() {
        use crate::app::Status;
        use crate::sessions::{Session, Sessions};
        let mut sessions = Sessions::new(test_app());
        let app = test_app();
        sessions.list.push(Session {
            label: "auth".to_string(),
            app,
        });
        // A run in the session off screen; it fails against the empty registry.
        sessions.list[1].app.start_run("work");
        while sessions.any_running() {
            // A time limit turns "never read" into a failure instead of a hang.
            let wait = sessions.next_event();
            let (index, event) = tokio::time::timeout(std::time::Duration::from_secs(5), wait)
                .await
                .expect("no event from the session off screen");
            assert_eq!(index, 1, "only that session has events");
            sessions.list[index].app.on_event(event).await;
        }
        assert_eq!(sessions.list[1].app.status(), Status::Failed);
        assert_eq!(sessions.active, 0, "reading events never switches");
    }

    #[tokio::test]
    async fn ready_events_share_one_render_even_beyond_tokios_receive_budget() {
        use crate::sessions::{Session, Sessions};
        use cupel_agent::agent_loop::agent_event_channel;
        use cupel_core::types::AssistantMessageEvent;

        let mut sessions = Sessions::new(test_app());
        sessions.list.push(Session {
            label: "auth".to_string(),
            app: test_app(),
        });
        let mut expected = Vec::new();
        for (index, session) in sessions.list.iter_mut().enumerate() {
            let (events, sink) = agent_event_channel();
            session.app.run_events = Some(events);
            sink.emit(AgentEvent::MessageUpdate {
                event: AssistantMessageEvent::ThinkingDelta {
                    content_index: 0,
                    delta: "planning".to_string(),
                },
            });
            let mut text = String::new();
            for delta_index in 0..256 {
                let delta = format!("{index}:{delta_index} ");
                text.push_str(&delta);
                sink.emit(AgentEvent::MessageUpdate {
                    event: AssistantMessageEvent::TextDelta {
                        content_index: 1,
                        delta,
                    },
                });
            }
            let ending = format!("batch complete {index}");
            text.push_str(&ending);
            sink.emit(AgentEvent::MessageUpdate {
                event: AssistantMessageEvent::TextDelta {
                    content_index: 1,
                    delta: ending,
                },
            });
            if index == 0 {
                sink.emit(AgentEvent::AgentEnd {
                    messages: Vec::new(),
                });
            }
            // The second stream closes without AgentEnd; both endings must be handled.
            expected.push(text);
        }

        // The normal wakeup handles one event; the rest must precede the next draw.
        let (index, event) = sessions.next_event().await;
        sessions.list[index].app.on_event(event).await;
        tokio::time::timeout(
            std::time::Duration::from_secs(1),
            sessions.drain_ready_events(),
        )
        .await
        .expect("draining must stop when all sessions are idle");

        assert!(!sessions.any_running());
        assert_eq!(sessions.active, 0, "draining never switches sessions");
        for (session, text) in sessions.list.iter().zip(&expected) {
            assert!(matches!(
                session.app.transcript.cells.as_slice(),
                [Cell::Thinking { text: thinking }, Cell::Answer { text: answer }]
                    if thinking == "planning" && answer == text
            ));
            assert!(session.app.run_finished);
        }
        assert!(sessions.take_finished_runs());
        assert!(!sessions.take_finished_runs());
        let screen = draw_sessions(&mut sessions, 100, 20);
        assert!(screen.contains("batch complete 0"), "{screen}");
    }

    #[tokio::test]
    async fn draining_ready_events_leaves_open_and_idle_sources_usable() {
        use crate::sessions::{Session, Sessions};
        use cupel_agent::agent_loop::agent_event_channel;
        use cupel_core::types::AssistantMessageEvent;

        let mut sessions = Sessions::new(test_app());
        sessions.list.push(Session {
            label: "auth".to_string(),
            app: test_app(),
        });
        let (events, sink) = agent_event_channel();
        sessions.list[1].app.run_events = Some(events);

        for delta in ["first ", "second", " third"] {
            sink.emit(AgentEvent::MessageUpdate {
                event: AssistantMessageEvent::TextDelta {
                    content_index: 0,
                    delta: delta.to_string(),
                },
            });
            tokio::time::timeout(
                std::time::Duration::from_secs(1),
                sessions.drain_ready_events(),
            )
            .await
            .expect("draining must not wait for an open, empty stream");
            assert!(sessions.any_running());
        }
        assert!(sessions.list[0].app.transcript.cells.is_empty());
        assert_eq!(
            sessions.list[1].app.transcript.copy_text(0),
            Some("first second third")
        );
    }

    #[test]
    fn quitting_while_another_session_works_takes_a_second_attempt() {
        use crate::sessions::{Session, Sessions};
        let mut sessions = Sessions::new(test_app());
        let mut app = test_app();
        // A run that never ends: `_sink` keeps its channel open.
        let (events, _sink) = cupel_agent::agent_loop::agent_event_channel();
        app.run_events = Some(events);
        sessions.list.push(Session {
            label: "auth".to_string(),
            app,
        });

        assert!(!sessions.quit_requested(), "nobody asked");
        sessions.active_mut().should_quit = true;
        assert!(!sessions.quit_requested(), "the first attempt warns");
        assert!(has_notice(sessions.active(), "auth still working"));
        sessions.active_mut().should_quit = true;
        assert!(sessions.quit_requested(), "the second attempt quits");

        // Alone, the first attempt quits.
        let mut alone = Sessions::new(test_app());
        alone.active_mut().should_quit = true;
        assert!(alone.quit_requested());
    }

    #[tokio::test]
    async fn new_rebuilds_only_the_session_on_screen() {
        use crate::app::ReloadTarget;
        use crate::sessions::{Session, Sessions};
        let mut sessions = Sessions::new(test_app());
        let app = test_app();
        sessions.list.push(Session {
            label: "auth".to_string(),
            app,
        });
        sessions.active = 1;
        sessions.reload(ReloadTarget::New).await;
        assert_eq!(sessions.list[1].label, "auth", "same place, same label");
        assert_ne!(sessions.list[1].app.recorder.session_id(), "cupel-test");
        assert_eq!(sessions.list[0].app.recorder.session_id(), "cupel-test");
    }

    /// Draw `sessions` the way the event loop does and return the screen.
    fn draw_sessions(sessions: &mut crate::sessions::Sessions, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| render_sessions(frame, sessions))
            .unwrap();
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

    /// Read the repository JSON examples without enabling persistence in tests.
    fn test_app_with_example_themes() -> App {
        let mut app = test_app();
        app.meta.home = Some(std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."));
        let mut app = App::new(app.agent, app.meta, app.recorder);
        app.meta.home = None;
        app
    }

    #[test]
    fn an_opaque_theme_leaves_no_cell_in_the_terminal_colors() {
        use crate::sessions::{Session, Sessions};
        for theme in crate::theme::example_themes().iter().skip(1) {
            let mut sessions = Sessions::new(test_app());
            sessions.theme = Arc::clone(theme);
            sessions.list.push(Session {
                label: "auth".into(),
                app: test_app(),
            });
            let app = sessions.active_mut();
            app.transcript.cells.push(Cell::User {
                text: "the task".into(),
            });
            app.transcript.cells.push(Cell::Answer {
                text: "```rust\nfn main() {}\n```".into(),
            });
            // Exercise Clear in the command popup as well as the sidebar.
            type_text(app, "/");
            let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
            terminal
                .draw(|frame| render_sessions(frame, &mut sessions))
                .unwrap();
            let buffer = terminal.backend().buffer();
            let screen: String = buffer
                .content
                .iter()
                .map(ratatui::buffer::Cell::symbol)
                .collect();
            assert!(screen.contains("help"), "{screen}");
            assert!(screen.contains("sessions"), "{screen}");
            for cell in &buffer.content {
                assert_ne!(cell.bg, Color::Reset, "{}: {screen}", theme.id);
                assert_ne!(cell.fg, Color::Reset, "{}: {screen}", theme.id);
            }
        }
    }

    #[test]
    fn themes_command_is_handled_locally() {
        use crate::app::ThemeRequest;
        for command in [
            "/themes nightfox",
            "/themes",
            "/themes dracula",
            "/themes nightfox extra",
        ] {
            let mut app = test_app_with_example_themes();
            submit_command(&mut app, command);
            match command {
                "/themes nightfox" => assert!(matches!(
                    &app.pending_theme, Some(ThemeRequest::Apply(theme)) if theme.id == "nightfox"
                )),
                "/themes" => assert!(matches!(app.pending_theme, Some(ThemeRequest::List))),
                _ => {
                    assert!(app.pending_theme.is_none());
                    assert_eq!(
                        last_notice(&app),
                        format!(
                            "unknown theme: {} (/themes lists them)",
                            command.trim_start_matches("/themes ")
                        )
                    );
                }
            }
            assert!(app.pending_prompt.is_none());
            assert!(!app.is_running());
            assert!(
                !app.transcript
                    .cells
                    .iter()
                    .any(|cell| matches!(cell, Cell::User { .. }))
            );
        }
    }

    #[test]
    fn themes_arguments_autocomplete_from_the_registry() {
        let mut app = test_app();
        type_text(&mut app, "/themes ");
        let (rows, _) = app.autocomplete.visible().expect("default theme row");
        assert_eq!(
            rows.len(),
            1,
            "without theme files, only CUPEL is available"
        );
        assert_eq!(rows[0].value, "cupel");

        let mut app = test_app_with_example_themes();
        type_text(&mut app, "/themes ");
        let (rows, _) = app.autocomplete.visible().expect("theme rows");
        let values: Vec<&str> = rows.iter().map(|row| row.value.as_str()).collect();
        assert_eq!(
            values[..6],
            [
                "cupel",
                "ayu-dark",
                "ayu-light",
                "dawnfox",
                "duskfox",
                "nightfox"
            ]
        );
        assert!(rows[5].display.contains("Nightfox - opaque (dark)"));
    }

    /// Style of the first ASCII occurrence, using the event loop's render path.
    fn sessions_style_of(
        sessions: &mut crate::sessions::Sessions,
        needle: &str,
    ) -> ratatui::style::Style {
        // Keep the old transcript budget after adding the model status row.
        let mut terminal = Terminal::new(TestBackend::new(100, 31)).unwrap();
        terminal
            .draw(|frame| render_sessions(frame, sessions))
            .unwrap();
        let buffer = terminal.backend().buffer();
        for y in 0..buffer.area.height {
            let row: String = (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect();
            if let Some(col) = row.find(needle) {
                return buffer[(u16::try_from(col).unwrap(), y)].style();
            }
        }
        panic!("needle {needle:?} not on screen");
    }

    #[test]
    fn switching_themes_restyles_old_messages_and_lists_the_active_one() {
        use crate::app::ThemeRequest;
        use crate::sessions::Sessions;
        let app = test_app_with_example_themes();
        let nightfox = crate::theme::find(&app.themes, "nightfox").unwrap();
        let mut sessions = Sessions::new(app);
        sessions.active_mut().transcript.cells.push(Cell::User {
            text: "the task".into(),
        });
        sessions.active_mut().transcript.cells.push(Cell::Answer {
            text: "the answer with `code`\n```\ncode panel\n```".into(),
        });
        assert_eq!(
            sessions_style_of(&mut sessions, "the task").bg,
            CUPEL.task.bg
        );

        sessions.on_theme_request(ThemeRequest::Apply(Arc::clone(&nightfox)));
        assert_eq!(sessions.theme.id, "nightfox");
        let notice = last_notice(sessions.active());
        assert!(
            notice.starts_with("theme switched to Nightfox - opaque"),
            "{notice}"
        );
        let task = sessions_style_of(&mut sessions, "the task");
        assert_eq!((task.fg, task.bg), (nightfox.task.fg, nightfox.task.bg));
        assert_eq!(
            sessions_style_of(&mut sessions, "the answer").fg,
            nightfox.answer.fg
        );
        assert_eq!(
            sessions_style_of(&mut sessions, "code panel").bg,
            nightfox.md_code_block.bg
        );
        assert_eq!(
            sessions_style_of(&mut sessions, "code panel").fg,
            nightfox.base.fg
        );
        assert_eq!(
            sessions_style_of(&mut sessions, "version:").fg,
            nightfox.chrome.fg
        );
        assert_eq!(
            sessions_style_of(&mut sessions, "Test Model").fg,
            nightfox.chrome.fg
        );

        sessions.on_theme_request(ThemeRequest::List);
        let listing = last_notice(sessions.active());
        assert!(listing.starts_with("themes (/themes <id>):"), "{listing}");
        assert!(
            listing.contains("* nightfox   Nightfox - opaque (dark)"),
            "{listing}"
        );
        assert!(
            listing.contains("  cupel      Cupel (terminal colors)"),
            "{listing}"
        );
        // Returning to the terminal theme must not retain opaque colors.
        sessions.on_theme_request(ThemeRequest::Apply(Arc::new(CUPEL)));
        assert_eq!(
            sessions_style_of(&mut sessions, "the task").bg,
            CUPEL.task.bg
        );
        assert_eq!(
            sessions_style_of(&mut sessions, "version:").bg,
            Some(Color::Reset)
        );
    }

    #[test]
    fn a_saved_theme_survives_a_restart() {
        use crate::app::ThemeRequest;
        use crate::sessions::Sessions;
        let mut sessions = Sessions::new(test_app());
        let examples = crate::theme::example_themes();
        let ayu_light = crate::theme::find(&examples, "ayu-light").unwrap();
        let duskfox = crate::theme::find(&examples, "duskfox").unwrap();
        sessions.on_theme_request(ThemeRequest::Apply(Arc::clone(&ayu_light)));
        assert_eq!(sessions.theme.id, "ayu-light");
        let notice = last_notice(sessions.active());
        assert!(notice.contains("for this run only - not saved"), "{notice}");

        let home = std::env::temp_dir().join("cupel-ui-theme-restart");
        let _ = std::fs::remove_dir_all(&home);
        let mut app = test_app_with_example_themes();
        app.meta.home = Some(home.clone());
        let mut sessions = Sessions::new(app);
        sessions.on_theme_request(ThemeRequest::Apply(ayu_light));
        let notice = last_notice(sessions.active());
        assert!(notice.contains("saved to"), "{notice}");

        let mut restarted = test_app_with_example_themes();
        restarted.meta.settings = cupel_coding_agent::settings::load_home_settings(Some(&home)).0;
        assert_eq!(Sessions::new(restarted).theme.id, "ayu-light");

        // A malformed file is not overwritten, but the live theme still switches.
        let path = home.join("settings.json");
        std::fs::write(&path, "{broken").unwrap();
        sessions.on_theme_request(ThemeRequest::Apply(duskfox));
        assert_eq!(sessions.theme.id, "duskfox");
        assert!(last_notice(sessions.active()).contains("for this run only - not saved"));
        assert_eq!(std::fs::read_to_string(path).unwrap(), "{broken");
        std::fs::remove_dir_all(home).unwrap();
    }

    #[test]
    fn an_unknown_theme_setting_warns_and_starts_with_the_default() {
        let mut app = test_app();
        app.meta.settings.theme = Some("dracula".into());
        let sessions = crate::sessions::Sessions::new(app);
        assert_eq!(sessions.theme.id, "cupel");
        assert!(has_notice(sessions.active(), "unknown theme \"dracula\""));
    }

    #[tokio::test]
    async fn the_theme_survives_new_and_session_switches() {
        use crate::app::{ReloadTarget, ThemeRequest};
        use crate::sessions::{Session, Sessions};
        let app = test_app_with_example_themes();
        let duskfox = crate::theme::find(&app.themes, "duskfox").unwrap();
        let mut sessions = Sessions::new(app);
        sessions.list.push(Session {
            label: "auth".into(),
            app: test_app(),
        });
        sessions.on_theme_request(ThemeRequest::Apply(duskfox));
        sessions.on_terminal_event(Event::Key(KeyEvent::new(
            KeyCode::Char('n'),
            KeyModifiers::CONTROL,
        )));
        assert_eq!(sessions.active, 1);
        assert_eq!(sessions.theme.id, "duskfox");
        sessions.reload(ReloadTarget::New).await;
        assert_eq!(sessions.theme.id, "duskfox");
        sessions.reload(ReloadTarget::Current).await;
        assert_eq!(sessions.theme.id, "duskfox");
    }

    #[tokio::test]
    async fn theme_files_are_listed_switched_to_and_reloaded() {
        use crate::app::{ReloadTarget, ThemeRequest};
        use crate::sessions::Sessions;

        let root =
            std::env::temp_dir().join(format!("cupel-ui-theme-files-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let dir = root.join("home/themes");
        std::fs::create_dir_all(&dir).unwrap();
        let gruvbox = r##"{"name": "Gruvbox Dark", "appearance": "dark", "palette": {
            "bg": "BG", "fg": "#ebdbb2", "surface": "#1d2021", "raised": "#3c3836",
            "selection": "#504945", "border": "#665c54", "muted": "#928374",
            "red": "#fb4934", "green": "#b8bb26", "yellow": "#fabd2f",
            "magenta": "#d3869b", "cyan": "#8ec07c",
            "heading": "#b8bb26", "emphasis": "#fe8019", "link": "#83a598"}}"##;
        let file = dir.join("gruvbox-dark.json");
        std::fs::write(&file, gruvbox.replace("BG", "#282828")).unwrap();
        std::fs::write(dir.join("broken.json"), "{").unwrap();

        let mut app = test_app_with_home(&root, "cupel-theme-files");
        assert!(has_notice(
            &app,
            &format!("ignoring theme file {}:", dir.join("broken.json").display())
        ));
        type_text(&mut app, "/themes gruv");
        let (rows, _) = app.autocomplete.visible().expect("file theme completion");
        assert_eq!(rows[0].value, "gruvbox-dark");
        app.input = crate::input::InputState::default();
        let mut sessions = Sessions::new(app);
        submit_command(sessions.active_mut(), "/themes gruvbox-dark");
        let request = sessions
            .active_mut()
            .pending_theme
            .take()
            .expect("a request");
        sessions.on_theme_request(request);
        assert_eq!(sessions.theme.id, "gruvbox-dark");
        assert_eq!(sessions.theme.base.bg, Some(Color::Rgb(0x28, 0x28, 0x28)));

        sessions.on_theme_request(ThemeRequest::List);
        let listing = last_notice(sessions.active());
        assert!(
            listing.contains(&format!(
                "* gruvbox-dark Gruvbox Dark (dark) - {}",
                file.display()
            )),
            "{listing}"
        );

        // The App that /hot-reload builds reads the files again.
        std::fs::write(&file, gruvbox.replace("BG", "#1d2021")).unwrap();
        sessions.reload(ReloadTarget::Current).await;
        assert_eq!(sessions.theme.base.bg, Some(Color::Rgb(0x1d, 0x20, 0x21)));

        // The saved file theme is also available after a restart.
        let mut app = test_app_with_home(&root, "cupel-theme-files-restart");
        app.meta.settings =
            cupel_coding_agent::settings::load_home_settings(app.meta.home.as_deref()).0;
        let restarted = Sessions::new(app);
        assert_eq!(restarted.theme.id, "gruvbox-dark");
        assert_eq!(restarted.theme.base.bg, sessions.theme.base.bg);

        // A missing active file keeps its current styles until a switch,
        // but cannot be selected on the next start.
        std::fs::remove_file(file).unwrap();
        sessions.reload(ReloadTarget::New).await;
        assert_eq!(sessions.theme.id, "gruvbox-dark");
        assert_eq!(sessions.theme.base.bg, Some(Color::Rgb(0x1d, 0x20, 0x21)));
        assert!(crate::theme::find(&sessions.active().themes, "gruvbox-dark").is_none());
        let mut app = test_app_with_home(&root, "cupel-theme-files-missing");
        app.meta.settings =
            cupel_coding_agent::settings::load_home_settings(app.meta.home.as_deref()).0;
        let restarted = Sessions::new(app);
        assert_eq!(restarted.theme.id, "cupel");
        assert!(has_notice(
            restarted.active(),
            "unknown theme \"gruvbox-dark\""
        ));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn the_sidebar_lists_every_session_once_there_are_two() {
        use crate::sessions::{Session, Sessions};
        let mut sessions = Sessions::new(test_app());
        let screen = draw_sessions(&mut sessions, 100, 20);
        assert!(!screen.contains(" sessions "), "one session: no sidebar");

        let mut app = test_app();
        // A run that never ends: `_sink` keeps its channel open.
        let (events, _sink) = cupel_agent::agent_loop::agent_event_channel();
        app.run_events = Some(events);
        sessions.list.push(Session {
            label: "auth".to_string(),
            app,
        });
        let screen = draw_sessions(&mut sessions, 100, 20);
        assert!(screen.contains(" sessions "), "{screen}");
        assert!(!screen.contains(" conversation "), "{screen}");
        assert!(screen.contains('┌') && screen.contains('└'), "{screen}");
        assert!(screen.contains(" ctrl+n/p · click "), "{screen}");
        for (y, row) in screen.lines().enumerate() {
            let main: String = row.chars().skip(usize::from(SIDEBAR_WIDTH)).collect();
            if matches!(y, 15 | 17) {
                assert_eq!(
                    main,
                    "─".repeat(100 - usize::from(SIDEBAR_WIDTH)),
                    "{screen}"
                );
            } else {
                assert!(!main.contains(['─', '│', '┌', '┐', '└', '┘']), "{screen}");
            }
        }
        assert!(screen.contains("○ origin"), "{screen}");
        assert!(screen.contains("◓ auth"), "{screen}");
        // The spinner turns with the ticks.
        sessions.tick();
        assert!(draw_sessions(&mut sessions, 100, 20).contains("◑ auth"));
        // Too narrow: the sidebar steps aside.
        let narrow = draw_sessions(&mut sessions, 79, 20);
        assert!(!narrow.contains(" sessions "), "{narrow}");
    }

    #[test]
    fn a_click_on_a_sidebar_row_shows_that_session() {
        use crate::sessions::{Session, Sessions};
        use ratatui::crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let mut sessions = Sessions::new(test_app());
        let app = test_app();
        sessions.list.push(Session {
            label: "auth".to_string(),
            app,
        });
        let click = |column, row| {
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column,
                row,
                modifiers: KeyModifiers::NONE,
            })
        };

        let screen = draw_sessions(&mut sessions, 100, 20);
        let row = screen
            .lines()
            .position(|line| line.contains("○ auth"))
            .unwrap();
        sessions.on_terminal_event(click(5, u16::try_from(row).unwrap()));
        assert_eq!(sessions.active, 1);
        // A click outside the sidebar belongs to the session on screen.
        draw_sessions(&mut sessions, 100, 20);
        sessions.on_terminal_event(click(60, 2));
        assert_eq!(sessions.active, 1);
    }

    #[test]
    fn long_labels_are_cut_to_fit() {
        assert_eq!(fit("auth", 18), "auth");
        assert_eq!(fit("abcdefghijklmnopqrstuvwx", 18), "abcdefghijklmnopq…");
    }

    const TEN_LINES: &str = "1\n2\n3\n4\n5\n6\n7\n8\n9\n10\n";

    /// A git repository with one empty commit on `main`, for the `/spinoff`
    /// tests. The path is canonical, like the paths git prints.
    fn git_repo(name: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("cupel-ui-spinoff-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let root = root.canonicalize().unwrap();
        std::fs::write(root.join("a.txt"), TEN_LINES).unwrap();
        let identity = ["-c", "user.name=test", "-c", "user.email=test@localhost"];
        for args in [
            &["init", "--quiet", "--initial-branch", "main"][..],
            &["config", "user.name", "test"][..],
            &["config", "user.email", "test@localhost"][..],
            &["config", "commit.gpgsign", "false"][..],
            &["add", "--all"][..],
            &["commit", "--quiet", "--message", "init"][..],
        ] {
            let status = std::process::Command::new("git")
                .arg("-C")
                .arg(&root)
                .args(identity)
                .args(["-c", "commit.gpgsign=false"])
                .args(args)
                .status()
                .unwrap();
            assert!(status.success(), "git {args:?}");
        }
        root
    }

    #[tokio::test]
    async fn spinoff_opens_a_session_in_its_own_worktree() {
        use crate::sessions::Sessions;
        use cupel_coding_agent::commands::SpinoffCommand;
        let repo = git_repo("create");
        let mut sessions = Sessions::new(test_app_in(repo.to_str().unwrap()));
        let create = SpinoffCommand::Create {
            name: "auth".parse().unwrap(),
            preset: None,
        };
        sessions.spinoff(create).await;

        assert_eq!(sessions.list.len(), 2);
        assert_eq!(sessions.active, 1, "the new session is on screen");
        assert_eq!(sessions.list[0].label, "main");
        assert_eq!(sessions.list[1].label, "auth");
        let worktree = repo.join(".cupel/worktrees/auth");
        assert!(worktree.is_dir());
        let cwd = worktree.display().to_string();
        assert_eq!(sessions.list[1].app.meta.cwd, cwd);
        assert!(has_notice(&sessions.list[1].app, "spinoff auth: worktree"));
        assert!(has_notice(&sessions.list[0].app, "spinoff auth started"));
        // Without a preset: the model of the session it came from.
        let model = |index: usize| sessions.list[index].app.agent.state().model.id;
        assert_eq!(model(1), model(0));

        // The listing shows it, with its session open.
        sessions.spinoff(SpinoffCommand::List).await;
        assert!(has_notice(sessions.active(), &format!("auth {cwd}")));
        assert!(!has_notice(sessions.active(), "no session open"));
    }

    #[tokio::test]
    async fn a_preset_spinoff_takes_the_preset_and_a_typo_creates_nothing() {
        use crate::sessions::Sessions;
        use cupel_coding_agent::commands::SpinoffCommand;
        use cupel_core::types::ThinkingLevel;
        let repo = git_repo("preset");
        let mut origin = test_app_in(repo.to_str().unwrap());
        origin.meta.settings = serde_json::from_str(
            r#"{"model": {"fast": {"provider": "anthropic", "model": "claude-haiku-5-5",
                "thinkingLevel": "low", "prompt": "Answer in one sentence."}}}"#,
        )
        .unwrap();
        let mut sessions = Sessions::new(origin);

        // A typo in the preset is refused before git runs.
        let typo = SpinoffCommand::Create {
            name: "docs".parse().unwrap(),
            preset: Some("fats".to_string()),
        };
        sessions.spinoff(typo).await;
        assert!(has_notice(sessions.active(), "unknown preset: fats"));
        assert!(!repo.join(".cupel/worktrees/docs").exists());
        assert_eq!(sessions.list.len(), 1);

        let fast = SpinoffCommand::Create {
            name: "docs".parse().unwrap(),
            preset: Some("fast".to_string()),
        };
        sessions.spinoff(fast).await;
        let state = sessions.active().agent.state();
        assert_eq!(state.model.id, "claude-haiku-5-5");
        assert_eq!(state.thinking_level, Some(ThinkingLevel::Low));
        assert!(state.system_prompt.ends_with("\n\nAnswer in one sentence."));
    }

    #[tokio::test]
    async fn spinoff_outside_a_repository_says_why_not() {
        use crate::sessions::Sessions;
        use cupel_coding_agent::commands::SpinoffCommand;
        let dir = std::env::temp_dir().join("cupel-ui-spinoff-no-repo");
        std::fs::create_dir_all(&dir).unwrap();
        let mut sessions = Sessions::new(test_app_in(dir.to_str().unwrap()));
        let create = SpinoffCommand::Create {
            name: "auth".parse().unwrap(),
            preset: None,
        };
        sessions.spinoff(create).await;
        assert!(has_notice(sessions.active(), "git repository"));
        assert_eq!(sessions.list.len(), 1);
    }

    #[tokio::test]
    async fn the_end_of_a_run_is_flagged_for_the_conflict_check() {
        let mut app = test_app();
        assert!(!app.run_finished);
        // A run against the empty registry ends quickly, with an error.
        app.start_run("hello");
        while app.is_running() {
            let event = app.next_event().await;
            app.on_event(event).await;
        }
        assert!(app.run_finished);
    }

    #[test]
    fn finished_runs_are_taken_once() {
        use crate::sessions::{Session, Sessions};
        let mut sessions = Sessions::new(test_app());
        let app = test_app();
        sessions.list.push(Session {
            label: "auth".to_string(),
            app,
        });
        assert!(!sessions.take_finished_runs());
        sessions.list[0].app.run_finished = true;
        sessions.list[1].app.run_finished = true;
        assert!(sessions.take_finished_runs());
        assert!(!sessions.take_finished_runs(), "every end counts once");
    }

    #[tokio::test]
    async fn conflicts_between_sessions_become_notices_and_go_away() {
        use crate::sessions::Sessions;
        use cupel_coding_agent::commands::SpinoffCommand;
        let repo = git_repo("conflicts");
        let mut sessions = Sessions::new(test_app_in(repo.to_str().unwrap()));
        let create = SpinoffCommand::Create {
            name: "auth".parse().unwrap(),
            preset: None,
        };
        sessions.spinoff(create).await;
        let worktree = repo.join(".cupel/worktrees/auth");
        let set_line_5 = |dir: &std::path::Path, text: &str| {
            let changed = TEN_LINES.replace("5\n", &format!("{text}\n"));
            std::fs::write(dir.join("a.txt"), changed).unwrap();
        };

        // Both sides change line 5 of a.txt.
        set_line_5(&repo, "5 origin");
        set_line_5(&worktree, "5 auth");
        sessions.check_conflicts().await;
        assert!(has_notice(
            &sessions.list[0].app,
            "conflict with auth: a.txt"
        ));
        assert!(has_notice(
            &sessions.list[1].app,
            "conflict with main: a.txt"
        ));
        assert_eq!(sessions.conflict_count(0), 1);
        assert_eq!(sessions.conflict_count(1), 1);

        // The same result again: no new notice.
        let cells = sessions.list[0].app.transcript.cells.len();
        sessions.check_conflicts().await;
        assert_eq!(sessions.list[0].app.transcript.cells.len(), cells);

        // The spinoff takes its change back: the conflict is gone.
        std::fs::write(worktree.join("a.txt"), TEN_LINES).unwrap();
        sessions.check_conflicts().await;
        assert!(has_notice(
            &sessions.list[0].app,
            "no more conflicts with auth"
        ));
        assert!(has_notice(
            &sessions.list[1].app,
            "no more conflicts with main"
        ));
        assert_eq!(sessions.conflict_count(0), 0);
    }

    #[test]
    fn the_sidebar_counts_conflicting_files() {
        use crate::sessions::{Session, Sessions};
        use cupel_coding_agent::spinoff::{Conflict, Side};
        let mut sessions = Sessions::new(test_app());
        let app = test_app();
        sessions.list.push(Session {
            label: "auth".to_string(),
            app,
        });
        let row = |screen: &str, label: &str| {
            let needle = format!("○ {label}");
            screen
                .lines()
                .find(|line| line.contains(&needle))
                .unwrap()
                .to_string()
        };
        let screen = draw_sessions(&mut sessions, 100, 20);
        assert!(!row(&screen, "auth").contains('!'), "{screen}");

        let auth = Side::Spinoff("auth".to_string());
        sessions.conflicts = vec![
            Conflict {
                sides: (Side::Origin, auth.clone()),
                files: vec!["a.txt".to_string(), "b.rs".to_string()],
            },
            // A spinoff without a session here still counts for auth, but
            // a.txt is already counted.
            Conflict {
                sides: (auth, Side::Spinoff("docs".to_string())),
                files: vec!["a.txt".to_string()],
            },
        ];
        let screen = draw_sessions(&mut sessions, 100, 20);
        assert!(row(&screen, "origin").contains(" !2"), "{screen}");
        assert!(row(&screen, "auth").contains(" !2"), "{screen}");
    }

    /// `a.txt` in `dir` with line `line` of [`TEN_LINES`] replaced by `text`.
    fn set_line(dir: &std::path::Path, line: usize, text: &str) {
        let changed = TEN_LINES.replace(&format!("{line}\n"), &format!("{text}\n"));
        std::fs::write(dir.join("a.txt"), changed).unwrap();
    }

    /// `git <args>` in `dir`; its output, for checks.
    fn git_output(dir: &std::path::Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .unwrap();
        assert!(output.status.success(), "git {args:?}");
        String::from_utf8_lossy(&output.stdout).into_owned()
    }

    /// `/spinoff <args>` in the session on screen, as the event loop runs it.
    async fn run_spinoff(sessions: &mut crate::sessions::Sessions, args: &str) {
        sessions.spinoff(args.parse().unwrap()).await;
    }

    #[tokio::test]
    async fn spinoff_merge_commits_merges_and_archives() {
        use crate::sessions::Sessions;
        let repo = git_repo("merge");
        let home = std::env::temp_dir().join("cupel-ui-spinoff-merge-home");
        let _ = std::fs::remove_dir_all(&home);
        let mut origin = test_app_in(repo.to_str().unwrap());
        origin.meta.home = Some(home.clone());
        let mut sessions = Sessions::new(origin);
        run_spinoff(&mut sessions, "auth").await;
        // A first prompt in auth, for its transcript. The run itself fails at once
        // against the empty registry.
        let auth = &mut sessions.list[1].app;
        auth.start_run("number line 9");
        while auth.is_running() {
            let event = auth.next_event().await;
            auth.on_event(event).await;
        }
        set_line(&repo.join(".cupel/worktrees/auth"), 9, "9 auth");

        // auth is on screen, but merging is the origin's job.
        run_spinoff(&mut sessions, "merge auth").await;
        assert!(has_notice(
            sessions.active(),
            "run in main, the origin session"
        ));
        sessions.active = 0;
        run_spinoff(&mut sessions, "merge auth").await;

        assert!(has_notice(
            sessions.active(),
            "spinoff auth is merged into main"
        ));
        assert_eq!(sessions.list.len(), 1, "auth's session is closed");
        assert!(!repo.join(".cupel/worktrees/auth").exists());
        let merged = std::fs::read_to_string(repo.join("a.txt")).unwrap();
        assert_eq!(merged, TEN_LINES.replace("9\n", "9 auth\n"));
        // auth's leftovers were committed under its first prompt.
        let subjects = git_output(&repo, &["log", "--format=%s"]);
        assert!(
            subjects.contains("spinoff auth: number line 9"),
            "{subjects}"
        );
        // Its transcript is in the origin's archive now.
        let sessions_dir = cupel_coding_agent::session::sessions_dir(Some(&home), &repo);
        let archive = sessions_dir.unwrap().join("archive");
        assert_eq!(std::fs::read_dir(archive).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn a_conflicted_merge_goes_to_the_origin_and_ends_with_the_next_merge() {
        use crate::sessions::Sessions;
        let repo = git_repo("merge-conflict");
        let mut sessions = Sessions::new(test_app_in(repo.to_str().unwrap()));
        run_spinoff(&mut sessions, "auth").await;
        sessions.active = 0;
        set_line(&repo.join(".cupel/worktrees/auth"), 5, "5 auth");
        set_line(&repo, 5, "5 main");
        git_output(
            &repo,
            &["commit", "--quiet", "--all", "--message", "main work"],
        );

        run_spinoff(&mut sessions, "merge auth").await;
        assert!(has_notice(
            sessions.active(),
            "merging auth stopped at conflicts in a.txt"
        ));
        let prompt = sessions
            .active_mut()
            .pending_prompt
            .take()
            .unwrap_or_default();
        let head = "Merging spinoff \"auth\" (branch cupel/spinoff/auth) into main stopped";
        assert!(prompt.starts_with(head), "{prompt}");
        // The leftovers commit is in the log; auth never had a prompt.
        assert!(prompt.contains(" spinoff auth: (no prompt)\n"), "{prompt}");
        assert!(prompt.contains("\n- a.txt\n"), "{prompt}");
        assert_eq!(sessions.list.len(), 2, "auth stays until the merge is done");

        // Markers left: refused, and no second prompt.
        run_spinoff(&mut sessions, "merge auth").await;
        assert!(has_notice(
            sessions.active(),
            "conflict markers are left in a.txt"
        ));
        assert!(sessions.active().pending_prompt.is_none());

        // The model's resolution; the next merge finishes and cleans up.
        set_line(&repo, 5, "5 main and auth");
        run_spinoff(&mut sessions, "merge auth").await;
        assert!(has_notice(
            sessions.active(),
            "spinoff auth is merged into main"
        ));
        assert_eq!(sessions.list.len(), 1);
        assert_eq!(git_output(&repo, &["status", "--porcelain"]), "");
    }

    #[tokio::test]
    async fn spinoff_drop_leaves_the_project_as_it_was() {
        use crate::sessions::Sessions;
        use cupel_coding_agent::spinoff::{Conflict, Side};
        let repo = git_repo("drop");
        let mut sessions = Sessions::new(test_app_in(repo.to_str().unwrap()));
        run_spinoff(&mut sessions, "auth").await;
        sessions.active = 0;
        set_line(&repo.join(".cupel/worktrees/auth"), 9, "9 dropped");
        let head = git_output(&repo, &["rev-parse", "HEAD"]);
        // A conflict with auth, as the last check found it.
        sessions.conflicts = vec![Conflict {
            sides: (Side::Origin, Side::Spinoff("auth".to_string())),
            files: vec!["a.txt".to_string()],
        }];

        run_spinoff(&mut sessions, "drop nope").await;
        assert!(has_notice(sessions.active(), "no spinoff named nope"));
        run_spinoff(&mut sessions, "drop auth").await;

        assert_eq!(sessions.list.len(), 1);
        assert!(!repo.join(".cupel/worktrees/auth").exists());
        assert_eq!(git_output(&repo, &["rev-parse", "HEAD"]), head);
        assert_eq!(git_output(&repo, &["status", "--porcelain"]), "");
        assert_eq!(git_output(&repo, &["branch", "--list", "cupel/*"]), "");
        assert!(
            sessions.conflicts.is_empty(),
            "auth's conflicts went with it"
        );
        // The notice names the commit that brings the work back.
        let notice = sessions
            .active()
            .transcript
            .cells
            .iter()
            .find_map(|cell| match cell {
                Cell::Notice { text } if text.contains("is dropped") => Some(text.clone()),
                _ => None,
            })
            .unwrap();
        let words: Vec<&str> = notice.split_whitespace().collect();
        let at = words.iter().position(|word| *word == "cupel/spinoff/auth");
        let commit = words[at.unwrap() + 1];
        let saved = git_output(&repo, &["show", &format!("{commit}:a.txt")]);
        assert!(saved.contains("9 dropped"), "{saved}");
    }

    #[tokio::test]
    async fn merge_and_drop_wait_for_working_agents() {
        use crate::sessions::Sessions;
        let repo = git_repo("merge-busy");
        let mut sessions = Sessions::new(test_app_in(repo.to_str().unwrap()));
        run_spinoff(&mut sessions, "auth").await;
        sessions.active = 0;
        // A run that never ends: `_sink` keeps its channel open.
        let (events, _sink) = cupel_agent::agent_loop::agent_event_channel();
        sessions.list[1].app.run_events = Some(events);
        run_spinoff(&mut sessions, "drop auth").await;
        assert!(has_notice(sessions.active(), "auth is still working"));

        // The same run, now in the origin.
        let events = sessions.list[1].app.run_events.take();
        sessions.list[0].app.run_events = events;
        run_spinoff(&mut sessions, "merge auth").await;
        assert!(has_notice(sessions.active(), "main is still working"));
        assert_eq!(sessions.list.len(), 2, "nothing removed");
        assert!(repo.join(".cupel/worktrees/auth").is_dir());
    }

    #[tokio::test]
    async fn inside_a_worktree_spinoff_commands_point_to_the_main_checkout() {
        use crate::sessions::Sessions;
        let repo = git_repo("inside");
        let mut sessions = Sessions::new(test_app_in(repo.to_str().unwrap()));
        run_spinoff(&mut sessions, "auth").await;

        // cupel started in auth's worktree, or one of its folders.
        let folder = repo.join(".cupel/worktrees/auth/src");
        std::fs::create_dir_all(&folder).unwrap();
        let mut inside = Sessions::new(test_app_in(folder.to_str().unwrap()));
        for args in ["docs", "", "merge auth", "drop auth"] {
            run_spinoff(&mut inside, args).await;
        }
        let refusal = "cupel runs in spinoff auth's worktree";
        let refusals = inside
            .active()
            .transcript
            .cells
            .iter()
            .filter(|cell| matches!(cell, Cell::Notice { text } if text.contains(refusal)))
            .count();
        assert_eq!(refusals, 4, "every /spinoff command is refused");
        assert!(!repo.join(".cupel/worktrees/docs").exists());
        assert!(repo.join(".cupel/worktrees/auth").is_dir());
    }

    #[tokio::test]
    async fn a_restart_restores_spinoffs_with_their_history_and_model() {
        use crate::sessions::Sessions;
        let repo = git_repo("restore");
        let home = std::env::temp_dir().join("cupel-ui-spinoff-restore-home");
        let _ = std::fs::remove_dir_all(&home);
        let origin = || {
            let mut origin = test_app_in(repo.to_str().unwrap());
            origin.meta.home = Some(home.clone());
            origin.meta.settings = serde_json::from_str(
                r#"{"model": {"fast": {"provider": "anthropic", "model": "claude-haiku-5-5",
                    "thinkingLevel": "low"}}}"#,
            )
            .unwrap();
            origin
        };
        // The first run: auth, on preset fast, gets one prompt; docs gets none. The
        // prompt's run fails at once against the empty registry, but it is recorded.
        let mut first = Sessions::new(origin());
        run_spinoff(&mut first, "auth fast").await;
        let auth = &mut first.list[1].app;
        auth.start_run("number line 9");
        while auth.is_running() {
            let event = auth.next_event().await;
            auth.on_event(event).await;
        }
        let auth_id = auth.recorder.session_id().to_string();
        let auth_history = auth.agent.state().messages.len();
        run_spinoff(&mut first, "docs").await;

        // The restart.
        let mut sessions = Sessions::new(origin());
        sessions.restore().await;
        assert_eq!(sessions.list.len(), 3);
        assert_eq!(sessions.list[0].label, "main");
        assert_eq!(sessions.active, 0, "the origin stays on screen");
        assert!(has_notice(
            sessions.active(),
            "spinoffs from the last run: "
        ));
        let app = |label: &str| {
            let session = sessions.list.iter().find(|session| session.label == label);
            &session.unwrap().app
        };
        let auth = app("auth");
        assert_eq!(
            auth.recorder.session_id(),
            auth_id,
            "its transcript continues"
        );
        assert_eq!(auth.agent.state().messages.len(), auth_history);
        assert!(has_notice(auth, "resumed session"));
        // The history replays its last answer, a failure here, so the row shows ✗.
        assert_eq!(auth.status(), crate::app::Status::Failed);
        // The model of its last answer, the thinking level of the origin.
        assert_eq!(auth.agent.state().model.id, "claude-haiku-5-5");
        let origin_thinking = sessions.list[0].app.agent.state().thinking_level;
        assert_eq!(auth.agent.state().thinking_level, origin_thinking);
        assert!(app("docs").agent.state().messages.is_empty());
    }

    #[tokio::test]
    async fn restore_keeps_a_single_session_where_there_is_nothing_to_restore() {
        use crate::sessions::Sessions;
        let no_repo = std::env::temp_dir().join("cupel-ui-spinoff-no-repo");
        std::fs::create_dir_all(&no_repo).unwrap();
        let repo = git_repo("restore-none");
        for dir in [&no_repo, &repo] {
            let mut sessions = Sessions::new(test_app_in(dir.to_str().unwrap()));
            sessions.restore().await;
            assert_eq!(sessions.list.len(), 1);
            assert!(sessions.active().transcript.cells.is_empty(), "no notice");
        }

        // Inside a spinoff's worktree, a notice says why there is only one session.
        let mut sessions = Sessions::new(test_app_in(repo.to_str().unwrap()));
        run_spinoff(&mut sessions, "auth").await;
        let worktree = repo.join(".cupel/worktrees/auth");
        let mut inside = Sessions::new(test_app_in(worktree.to_str().unwrap()));
        inside.restore().await;
        assert_eq!(inside.list.len(), 1);
        assert!(has_notice(
            inside.active(),
            "cupel runs in spinoff auth's worktree"
        ));
    }

    #[tokio::test]
    async fn restore_says_what_it_could_not_bring_back() {
        use crate::sessions::Sessions;
        use cupel_coding_agent::session::{TRANSCRIPT_VERSION, TranscriptHeader, sessions_dir};
        let repo = git_repo("restore-notes");
        let home = std::env::temp_dir().join("cupel-ui-spinoff-restore-notes-home");
        let _ = std::fs::remove_dir_all(&home);
        let mut sessions = Sessions::new(test_app_in(repo.to_str().unwrap()));
        for name in ["auth", "docs"] {
            run_spinoff(&mut sessions, name).await;
        }
        let transcript = |name: &str| {
            let worktree = repo.join(".cupel/worktrees").join(name);
            let dir = sessions_dir(Some(&home), &worktree).unwrap();
            std::fs::create_dir_all(&dir).unwrap();
            dir.join("cupel-1.jsonl")
        };
        // auth's first answer came from a model the catalog has, its last one from a
        // model it doesn't have ("mock").
        let header = TranscriptHeader {
            version: TRANSCRIPT_VERSION,
            session_id: "cupel-1".to_string(),
            cwd: "auth".to_string(),
            model: "mock".to_string(),
            started_at: 1,
        };
        let mut first = assistant_message("first", cupel_core::types::StopReason::Stop, 0);
        if let cupel_agent::AgentMessage::Llm(cupel_core::types::Message::Assistant(answer)) =
            &mut first
        {
            answer.provider = cupel_core::types::Provider::from("anthropic");
            answer.model = "claude-haiku-5-5".to_string();
        }
        let last = assistant_message("last", cupel_core::types::StopReason::Stop, 0);
        let text = format!(
            "{}\n{}\n{}\n",
            serde_json::to_string(&header).unwrap(),
            serde_json::to_string(&first).unwrap(),
            serde_json::to_string(&last).unwrap()
        );
        std::fs::write(transcript("auth"), text).unwrap();
        // docs's transcript is broken.
        std::fs::write(transcript("docs"), "{broken").unwrap();

        let mut origin = test_app_in(repo.to_str().unwrap());
        origin.meta.home = Some(home.clone());
        let mut restarted = Sessions::new(origin);
        restarted.restore().await;
        let app = |label: &str| {
            let session = restarted.list.iter().find(|session| session.label == label);
            &session.unwrap().app
        };
        let missing = "mock/mock, the model of the last answer, is not available";
        assert!(has_notice(app("auth"), missing));
        assert_eq!(app("auth").recorder.session_id(), "cupel-1");
        assert!(has_notice(app("docs"), "the session starts empty"));
        assert_ne!(
            app("docs").recorder.session_id(),
            "cupel-1",
            "a new transcript"
        );
    }

    #[tokio::test]
    async fn spinoff_merge_takes_a_commit_message() {
        use crate::sessions::Sessions;
        let repo = git_repo("merge-message");
        let mut sessions = Sessions::new(test_app_in(repo.to_str().unwrap()));
        run_spinoff(&mut sessions, "auth").await;
        sessions.active = 0;
        set_line(&repo.join(".cupel/worktrees/auth"), 9, "9 auth");

        run_spinoff(&mut sessions, r#"merge auth "Add line nine""#).await;
        assert!(has_notice(
            sessions.active(),
            "spinoff auth is merged into main"
        ));
        let subject = git_output(&repo, &["log", "-1", "--format=%s"]);
        assert_eq!(subject, "Add line nine\n");
    }

    fn press(app: &mut App, code: KeyCode) {
        app.on_terminal_event(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)));
    }

    /// The tool's arguments: two options, the second one recommended.
    fn presets_question() -> serde_json::Value {
        serde_json::json!({
            "question": "Where should presets live?",
            "options": [
                {"label": "settings.json", "description": "No new file.", "recommended": false},
                {"label": "presets.toml", "description": "A second loader.", "recommended": true}
            ]
        })
    }

    /// Put the question on screen the way the event loop does. The returned
    /// receiver is the end the tool would wait on.
    fn open_question(app: &mut App) -> tokio::sync::oneshot::Receiver<Answer> {
        let question = serde_json::from_value(presets_question()).unwrap();
        let (pending, answer) = PendingQuestion::new(question);
        app.on_question(Some(pending));
        answer
    }

    #[tokio::test]
    async fn an_open_question_shows_its_options_and_waits() {
        let mut app = test_app();
        app.start_run("plan the presets");
        let _answer = open_question(&mut app);
        let screen = draw(&mut app, 100, 30);
        assert!(screen.contains("? Where should presets live?"), "{screen}");
        assert!(screen.contains("  1. settings.json"), "{screen}");
        assert!(
            screen.contains("› 2. presets.toml (recommended)"),
            "{screen}"
        );
        assert!(screen.contains("A second loader."), "{screen}");
        assert!(screen.contains("Your answer"), "{screen}");
        assert!(
            screen.contains("enter answer (typed text wins)"),
            "{screen}"
        );
        assert_eq!(app.status(), Status::Asking);
    }

    #[tokio::test]
    async fn enter_sends_the_recommended_option_back_to_the_tool() {
        use cupel_agent::AgentTool as _;
        let mut app = test_app();
        let (tool, questions) = AskUserTool::new();
        app.questions = Some(questions);
        // The tool asks from its own task, the way it runs inside the agent loop.
        let ask = tokio::spawn(async move {
            let cancel = tokio_util::sync::CancellationToken::new();
            tool.execute("call_1", presets_question(), cancel, None)
                .await
        });
        let event = app.next_event().await;
        app.on_event(event).await;
        press(&mut app, KeyCode::Enter);
        // Checked before waiting on the tool: an Enter that didn't answer
        // fails here instead of waiting forever.
        assert!(app.question.is_none());

        let result = ask.await.unwrap().unwrap();
        assert_eq!(
            result,
            cupel_agent::types::AgentToolResult::text("The user picked option 2: presets.toml")
        );
    }

    #[test]
    fn up_and_down_stay_within_the_options() {
        let mut app = test_app();
        let mut answer = open_question(&mut app);
        // The highlight starts on option 2, the recommended one and the last.
        for key in [KeyCode::Down, KeyCode::Down, KeyCode::Up, KeyCode::Up] {
            press(&mut app, key);
        }
        press(&mut app, KeyCode::Enter);
        assert_eq!(answer.try_recv(), Ok(Answer::Picked(0)));
    }

    #[test]
    fn typed_text_is_the_answer() {
        let mut app = test_app();
        let mut answer = open_question(&mut app);
        type_text(&mut app, "env vars");
        press(&mut app, KeyCode::Enter);
        assert_eq!(answer.try_recv(), Ok(Answer::Typed("env vars".to_string())));
        assert!(app.input.is_empty());
        assert!(app.pending_prompt.is_none(), "an answer is no prompt");
    }

    #[test]
    fn modified_enter_answers_a_question_without_steering() {
        for code in [KeyCode::Enter, KeyCode::Char('j')] {
            let mut app = test_app();
            let mut answer = open_question(&mut app);
            type_text(&mut app, "neither");
            app.on_terminal_event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT)));
            assert!(app.question.is_some(), "Alt+Enter adds a line");
            type_text(&mut app, "use env vars");
            app.on_terminal_event(Event::Key(KeyEvent::new(code, KeyModifiers::CONTROL)));
            assert_eq!(
                answer.try_recv(),
                Ok(Answer::Typed("neither\nuse env vars".to_string()))
            );
            assert!(app.question.is_none());
            assert!(app.input.is_empty());
            assert!(app.pending_prompt.is_none());
            assert!(app.steering.is_empty());
            assert!(app.queued.is_empty());
        }
    }

    #[test]
    fn a_background_question_marks_its_session_in_the_sidebar() {
        use crate::sessions::{Session, Sessions};
        let mut sessions = Sessions::new(test_app());
        let mut app = test_app();
        let _answer = open_question(&mut app);
        sessions.list.push(Session {
            label: "auth".to_string(),
            app,
        });
        let screen = draw_sessions(&mut sessions, 100, 30);
        assert!(screen.contains("? auth"), "{screen}");
        assert!(!screen.contains("Where should presets live?"), "{screen}");
    }

    #[test]
    fn question_rows_use_the_active_theme() {
        let mut app = test_app();
        let _answer = open_question(&mut app);
        for theme in crate::theme::example_themes() {
            let lines = question_lines(&app, 100, &theme).lines;
            let style = lines[0].spans[0].style;
            assert_eq!(style.fg, theme.input_border_busy.fg);
            assert!(style.add_modifier.contains(Modifier::BOLD));
            assert_eq!(lines[2].spans[0].style, theme.popup_row);
            assert_eq!(lines[3].spans[0].style, theme.chrome);
            assert_eq!(lines[5].spans[0].style, theme.popup_selected);
        }
    }

    #[test]
    fn question_rows_wrap_and_strip_terminal_commands() {
        let mut app = test_app();
        let mut args = presets_question();
        args["question"] = serde_json::json!("\u{1b}[31mWhere should presets live?\u{1b}[0m");
        args["options"][0]["label"] = serde_json::json!("\u{1b}]52;c;payload\u{7}settings.json");
        args["options"][0]["description"] = serde_json::json!("No\tnew file.\nKeep the loader.");
        let (pending, mut answer) = PendingQuestion::new(serde_json::from_value(args).unwrap());
        app.on_question(Some(pending));
        let lines = question_lines(&app, 24, &CUPEL).lines;
        let text = lines
            .iter()
            .map(Line::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("settings.json"), "{text}");
        assert!(text.contains("Keep the loader."), "{text}");
        assert!(!text.contains(['\u{1b}', '\u{7}', '\t']));
        assert!(!text.contains("payload"));
        assert!(lines.iter().all(|line| line.width() <= 24));
        for (width, height) in [(1, 1), (10, 6), (20, 10)] {
            draw(&mut app, width, height);
        }
        // Rendering changes no answer data.
        press(&mut app, KeyCode::Enter);
        assert_eq!(answer.try_recv(), Ok(Answer::Picked(1)));
    }

    #[test]
    fn a_question_pane_has_inside_padding_and_outside_gaps() {
        let mut app = test_app();
        let _answer = open_question(&mut app);
        let screen = draw(&mut app, 100, 30);
        assert!(!screen.contains(['│', '┌', '┐', '└', '┘']), "{screen}");
        let rows: Vec<&str> = screen.lines().collect();
        let top = rows
            .iter()
            .position(|row| row.contains("Decision required"))
            .unwrap();
        let bottom = rows
            .iter()
            .position(|row| row.chars().all(|ch| ch == '─'))
            .unwrap();
        let question = rows
            .iter()
            .position(|row| row.contains("? Where should presets live?"))
            .unwrap();
        assert!(rows[top - 1].trim().is_empty(), "{screen}");
        assert!(rows[top + 1].trim().is_empty(), "{screen}");
        assert_eq!(question, top + 2, "{screen}");
        assert!(rows[question].starts_with("  ? "), "{screen}");
        assert!(rows[question + 1].trim().is_empty(), "{screen}");
        assert!(rows[bottom - 1].trim().is_empty(), "{screen}");
        assert!(rows[bottom + 1].trim().is_empty(), "{screen}");
        assert!(rows[bottom + 2].contains("Test Model"), "{screen}");
        assert!(rows[bottom + 3].contains("Your answer"), "{screen}");
    }

    #[test]
    fn answering_removes_the_pane_and_restores_transcript_space() {
        let mut app = test_app();
        let mut answer = open_question(&mut app);
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        terminal
            .draw(|frame| render(frame, &mut app, frame.area(), &CUPEL))
            .unwrap();
        let asking_height = app.last_transcript_height;
        press(&mut app, KeyCode::Enter);
        assert_eq!(answer.try_recv(), Ok(Answer::Picked(1)));
        terminal
            .draw(|frame| render(frame, &mut app, frame.area(), &CUPEL))
            .unwrap();
        assert!(app.last_transcript_height > asking_height);
        // Inspect the same terminal's second frame, not a fresh blank buffer.
        let buffer = terminal.backend().buffer();
        let text: String = buffer
            .content
            .iter()
            .map(ratatui::buffer::Cell::symbol)
            .collect();
        assert!(!text.contains("Decision required"));
        assert!(!text.contains("Your answer"));
        assert!(!text.contains("presets.toml"));
        assert!(question_pane(&app, Rect::new(0, 0, 100, 15), &CUPEL).is_none());
    }

    #[test]
    fn a_capped_question_keeps_the_selected_label_visible_after_resizing() {
        let mut app = test_app();
        let mut args = presets_question();
        args["options"] = serde_json::json!(
            (1..=5)
                .map(|number| {
                    serde_json::json!({
                        "label": format!("choice-{number}"),
                        "description": "A long description. ".repeat(20),
                        "recommended": number == 5
                    })
                })
                .collect::<Vec<_>>()
        );
        let (pending, mut answer) = PendingQuestion::new(serde_json::from_value(args).unwrap());
        app.on_question(Some(pending));
        for (width, height) in [(80, 24), (60, 12), (100, 30)] {
            let screen = draw(&mut app, width, height);
            assert!(screen.contains("› 5. choice-5"), "{screen}");
            assert!(screen.contains(" rows "), "{screen}");
        }
        press(&mut app, KeyCode::Up);
        assert!(draw(&mut app, 80, 24).contains("› 4. choice-4"));
        press(&mut app, KeyCode::Enter);
        assert_eq!(answer.try_recv(), Ok(Answer::Picked(3)));
    }

    #[test]
    fn question_continuations_keep_their_display_column_indent() {
        let mut lines = Vec::new();
        push_question_text(
            &mut lines,
            "日本語の説明です。More words.\nAnother line.",
            "› 1. ",
            "     ",
            20,
            CUPEL.popup_selected,
        );
        assert!(lines.len() > 2);
        assert!(lines[0].to_string().starts_with("› 1. "));
        assert!(
            lines[1..]
                .iter()
                .all(|line| line.to_string().starts_with("     "))
        );
        assert!(lines.iter().all(|line| line.width() <= 20));
    }

    #[test]
    fn a_question_pane_uses_the_active_theme_for_border_and_title() {
        let mut app = test_app();
        let _answer = open_question(&mut app);
        for theme in crate::theme::example_themes() {
            let area = Rect::new(0, 0, 100, 15);
            let pane = question_pane(&app, area, &theme).unwrap();
            assert!(pane.height <= area.height);
            let mut terminal = Terminal::new(TestBackend::new(100, 15)).unwrap();
            terminal
                .draw(|frame| render_question(frame, pane, area, &theme))
                .unwrap();
            let buffer = terminal.backend().buffer();
            assert_eq!(
                buffer[(99, 0)].fg,
                theme.pane_border.fg.unwrap_or(Color::Reset)
            );
            assert_eq!(buffer[(2, 0)].fg, theme.question.fg.unwrap_or(Color::Reset));
            assert!(buffer[(2, 0)].modifier.contains(Modifier::BOLD));
        }
    }

    #[test]
    fn a_question_pane_degrades_without_panicking_in_tiny_areas() {
        let mut app = test_app();
        let _answer = open_question(&mut app);
        for (width, height) in [(1, 1), (2, 2), (10, 6), (20, 10)] {
            draw(&mut app, width, height);
        }
        let area = Rect::new(0, 0, 8, 2);
        let pane = question_pane(&app, area, &CUPEL).unwrap();
        assert_eq!(
            pane.block.inner(area),
            area,
            "no border or title consumes the body"
        );
        assert!(question_pane(&app, Rect::default(), &CUPEL).is_none());
    }

    #[tokio::test]
    async fn esc_aborts_the_run_and_takes_the_question_away() {
        use tokio::sync::oneshot::error::TryRecvError;
        let mut app = test_app();
        app.start_run("plan the presets");
        let mut answer = open_question(&mut app);
        press(&mut app, KeyCode::Esc);
        assert!(app.question.is_none());
        assert_eq!(answer.try_recv(), Err(TryRecvError::Closed));
        while app.is_running() {
            let event = app.next_event().await;
            app.on_event(event).await;
        }
    }

    #[tokio::test]
    async fn a_finished_run_closes_its_question() {
        let mut app = test_app();
        let _answer = open_question(&mut app);
        // The event stream closed without an AgentEnd.
        app.on_agent_event(None).await;
        assert!(app.question.is_none());
    }

    #[tokio::test]
    async fn a_closed_question_channel_stops_waking_the_loop() {
        let mut app = test_app();
        let (tool, questions) = AskUserTool::new();
        app.questions = Some(questions);
        drop(tool);
        let event = app.next_event().await;
        app.on_event(event).await;
        assert!(app.questions.is_none());
    }

    #[tokio::test]
    async fn reloaded_sessions_listen_for_questions() {
        use crate::app::ReloadTarget;
        // In place keeps the session; New opens one, like /spinoff does.
        for (name, target) in [
            ("current", ReloadTarget::Current),
            ("new", ReloadTarget::New),
        ] {
            let root = std::env::temp_dir().join(format!("cupel-ui-reload-questions-{name}"));
            let _ = std::fs::remove_dir_all(&root);
            let app = test_app_with_home(&root, "cupel-current");
            let app = app.hot_reload(target).await;
            assert!(app.questions.is_some(), "{name}");
        }
    }
}
