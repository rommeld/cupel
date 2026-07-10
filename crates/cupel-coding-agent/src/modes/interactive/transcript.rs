//! The transcript render model.
//!
//! Agent events describe *what happened*; the transcript describes *what to
//! draw*. Keeping a separate render model (a `Vec<Cell>`) instead of drawing
//! straight from `AgentMessage`s has two payoffs:
//!
//! 1. Streaming deltas mutate the LAST cell in place (append to the text
//!    being typed out) instead of re-deriving the whole view per event.
//! 2. UI-only state (tool results attached to their calls, expansion,
//!    "queued" markers) has an obvious home that the agent knows nothing
//!    about.
//!
//! pi's TUI does the same thing with its component tree; ratatui is
//! immediate-mode, so our "components" are plain data plus a `to_lines`
//! function.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthChar;

/// How many result lines a collapsed tool cell shows.
const TOOL_PREVIEW_LINES: usize = 6;

/// One visual block in the conversation.
pub enum Cell {
    /// A user message (submitted prompt or drained steering message).
    User { text: String },
    /// A message queued for steering while the agent is still running. It
    /// re-appears as a `User` cell when the loop drains it; this marker just
    /// shows the input wasn't lost in the meantime.
    Queued { text: String },
    /// Streaming assistant prose.
    Assistant { text: String },
    /// Streaming assistant thinking (rendered dim).
    Thinking { text: String },
    /// A tool call and (once finished) its result.
    Tool {
        /// Tool call id, used to attach the result when it completes.
        id: String,
        name: String,
        /// Compact JSON of the arguments (may still be growing during
        /// streaming; replaced by the final version on `ToolCallEnd`).
        args: String,
        result: Option<ToolOutcome>,
    },
    /// An error surfaced by the agent or a provider.
    Error { text: String },
    /// A status notice (e.g. "retrying in 2s"), rendered in warning color.
    Notice { text: String },
    /// Per-turn usage/cost summary.
    Usage { text: String },
}

pub struct ToolOutcome {
    pub text: String,
    pub is_error: bool,
}

#[derive(Default)]
pub struct Transcript {
    pub cells: Vec<Cell>,
}

impl Transcript {
    /// Append a delta to the last assistant cell, creating one if the last
    /// cell is something else (e.g. the first delta after a tool result).
    pub fn append_assistant(&mut self, delta: &str) {
        if let Some(Cell::Assistant { text }) = self.cells.last_mut() {
            text.push_str(delta);
        } else {
            self.cells.push(Cell::Assistant {
                text: delta.to_string(),
            });
        }
    }

    /// Same, for thinking deltas.
    pub fn append_thinking(&mut self, delta: &str) {
        if let Some(Cell::Thinking { text }) = self.cells.last_mut() {
            text.push_str(delta);
        } else {
            self.cells.push(Cell::Thinking {
                text: delta.to_string(),
            });
        }
    }

    /// Attach a finished result to its tool cell (matched by call id).
    pub fn attach_tool_result(&mut self, tool_call_id: &str, outcome: ToolOutcome) {
        // Search from the end: the matching call is almost always recent.
        for cell in self.cells.iter_mut().rev() {
            if let Cell::Tool { id, result, .. } = cell
                && id == tool_call_id
            {
                *result = Some(outcome);
                return;
            }
        }
    }

    /// Flatten every cell into styled, wrapped lines for a given terminal
    /// width. Called once per frame; cheap enough at chat-transcript sizes
    /// that we don't cache (ratatui diffs the actual terminal writes anyway).
    #[must_use]
    pub fn to_lines(&self, width: u16) -> Vec<Line<'static>> {
        let width = width.max(10) as usize;
        let mut out: Vec<Line<'static>> = Vec::new();

        for cell in &self.cells {
            // A blank spacer between blocks, but not at the very top.
            if !out.is_empty() {
                out.push(Line::default());
            }
            match cell {
                Cell::User { text } => {
                    push_wrapped(
                        &mut out,
                        &format!("> {text}"),
                        width,
                        Style::new().fg(Color::Green).add_modifier(Modifier::BOLD),
                    );
                }
                Cell::Queued { text } => {
                    push_wrapped(
                        &mut out,
                        &format!("(queued) {text}"),
                        width,
                        Style::new().fg(Color::Green).add_modifier(Modifier::DIM),
                    );
                }
                Cell::Assistant { text } => {
                    push_wrapped(&mut out, text, width, Style::default());
                }
                Cell::Thinking { text } => {
                    push_wrapped(
                        &mut out,
                        text,
                        width,
                        Style::new().add_modifier(Modifier::DIM | Modifier::ITALIC),
                    );
                }
                Cell::Tool {
                    name, args, result, ..
                } => {
                    push_wrapped(
                        &mut out,
                        &format!("[{name}] {args}"),
                        width,
                        Style::new().fg(Color::Cyan),
                    );
                    match result {
                        None => push_wrapped(
                            &mut out,
                            "  ...",
                            width,
                            Style::new().add_modifier(Modifier::DIM),
                        ),
                        Some(outcome) => {
                            let style = if outcome.is_error {
                                Style::new().fg(Color::Red)
                            } else {
                                Style::new().add_modifier(Modifier::DIM)
                            };
                            // Tool results show a fixed preview; the FULL
                            // output already went to the model (and to the
                            // trace log) - the transcript view is a digest.
                            let total = outcome.text.lines().count();
                            for line in outcome.text.lines().take(TOOL_PREVIEW_LINES) {
                                push_wrapped(&mut out, &format!("  {line}"), width, style);
                            }
                            if total > TOOL_PREVIEW_LINES {
                                push_wrapped(
                                    &mut out,
                                    &format!("  ... ({} more lines)", total - TOOL_PREVIEW_LINES),
                                    width,
                                    Style::new().add_modifier(Modifier::DIM),
                                );
                            }
                        }
                    }
                }
                Cell::Error { text } => {
                    push_wrapped(
                        &mut out,
                        &format!("error: {text}"),
                        width,
                        Style::new().fg(Color::Red),
                    );
                }
                Cell::Notice { text } => {
                    push_wrapped(&mut out, text, width, Style::new().fg(Color::Yellow));
                }
                Cell::Usage { text } => {
                    push_wrapped(
                        &mut out,
                        text,
                        width,
                        Style::new().add_modifier(Modifier::DIM),
                    );
                }
            }
        }
        out
    }
}

/// Wrap `text` to `width` display columns and append the resulting lines,
/// all sharing one style.
fn push_wrapped(out: &mut Vec<Line<'static>>, text: &str, width: usize, style: Style) {
    for logical in text.split('\n') {
        for chunk in wrap_line(logical, width) {
            out.push(Line::from(Span::styled(chunk, style)));
        }
    }
}

/// Greedy word wrap by display width.
///
/// Why not a crate: `textwrap` exists, but this is ~30 lines, teaches how
/// display-column math works (a CJK char occupies 2 columns), and gives us
/// the exact break behavior we want (hard-split words longer than the line).
#[must_use]
pub fn wrap_line(line: &str, width: usize) -> Vec<String> {
    if line.is_empty() {
        return vec![String::new()];
    }
    let mut out: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut current_width = 0_usize;

    // Split into "words" that keep their trailing spaces, so rejoining
    // preserves spacing exactly.
    for word in split_keeping_spaces(line) {
        let word_width: usize = word.chars().map(|c| c.width().unwrap_or(0)).sum();

        if current_width + word_width <= width {
            current.push_str(word);
            current_width += word_width;
            continue;
        }
        // The word doesn't fit on this line. Emit the line (if non-empty)
        // and start fresh.
        if !current.is_empty() {
            out.push(core::mem::take(&mut current));
            current_width = 0;
        }
        // A word longer than the whole line gets hard-split by columns.
        if word_width > width {
            for c in word.chars() {
                let w = c.width().unwrap_or(0);
                if current_width + w > width && !current.is_empty() {
                    out.push(core::mem::take(&mut current));
                    current_width = 0;
                }
                current.push(c);
                current_width += w;
            }
        } else {
            current.push_str(word);
            current_width = word_width;
        }
    }
    if !current.is_empty() || out.is_empty() {
        out.push(current);
    }
    out
}

/// Split `"foo bar  baz"` into `["foo ", "bar  ", "baz"]` - words own their
/// trailing whitespace so wrapping never eats spacing.
fn split_keeping_spaces(line: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0;
    let mut in_space = false;
    for (i, c) in line.char_indices() {
        if c == ' ' {
            in_space = true;
        } else if in_space {
            out.push(&line[start..i]);
            start = i;
            in_space = false;
        }
    }
    out.push(&line[start..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_short_line_passes_through() {
        assert_eq!(wrap_line("hello world", 20), vec!["hello world"]);
    }

    #[test]
    fn wrap_breaks_at_word_boundary() {
        assert_eq!(
            wrap_line("hello brave new world", 11),
            vec!["hello ", "brave new ", "world"]
        );
    }

    #[test]
    fn wrap_hard_splits_long_words() {
        assert_eq!(wrap_line("abcdefghij", 4), vec!["abcd", "efgh", "ij"]);
    }

    #[test]
    fn wrap_counts_wide_chars_as_two_columns() {
        // Each CJK char is 2 columns; 4 columns fit 2 chars.
        assert_eq!(wrap_line("日本語だ", 4), vec!["日本", "語だ"]);
    }

    #[test]
    fn wrap_empty_line_stays_a_line() {
        assert_eq!(wrap_line("", 10), vec![""]);
    }

    #[test]
    fn streaming_deltas_append_to_last_cell() {
        let mut transcript = Transcript::default();
        transcript.append_assistant("Hel");
        transcript.append_assistant("lo");
        assert_eq!(transcript.cells.len(), 1);
        let Some(Cell::Assistant { text }) = transcript.cells.last() else {
            panic!("expected assistant cell");
        };
        assert_eq!(text, "Hello");
    }

    #[test]
    fn thinking_then_text_makes_two_cells() {
        let mut transcript = Transcript::default();
        transcript.append_thinking("hmm");
        transcript.append_assistant("answer");
        assert_eq!(transcript.cells.len(), 2);
    }

    #[test]
    fn tool_result_attaches_by_id() {
        let mut transcript = Transcript::default();
        transcript.cells.push(Cell::Tool {
            id: "call_1".into(),
            name: "grep".into(),
            args: "{}".into(),
            result: None,
        });
        transcript.attach_tool_result(
            "call_1",
            ToolOutcome {
                text: "hit".into(),
                is_error: false,
            },
        );
        let Some(Cell::Tool {
            result: Some(outcome),
            ..
        }) = transcript.cells.last()
        else {
            panic!("expected tool cell with result");
        };
        assert_eq!(outcome.text, "hit");
    }
}
