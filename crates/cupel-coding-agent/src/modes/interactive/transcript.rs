//! The transcript render model.
//!
//! Agent events describe *what happened*; the transcript describes *what to
//! draw*. Keeping a separate render model (a `Vec<Cell>`) instead of drawing
//! straight from `AgentMessage`s has two payoffs:
//!
//! 1. Streaming deltas mutate the LAST cell in place (append to the text
//!    being typed out) instead of re-deriving the whole view per event.
//! 2. UI-only state (tool results attached to their calls, expansion
//!    ) has an obvious home that the agent knows nothing about.
//!
//! The view is TWO panes: conversation (user, reasoning, answers) on the
//! left, tool traffic on the right. [`Transcript::steps`] groups the flat
//! cell list into horizontal bands, and [`Transcript::to_columns`] renders
//! each band across the SAME rows in both panes - that row alignment (plus
//! a shared band number) is what assigns a tool call to its reasoning step.

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthChar;

use crate::modes::interactive::theme;

/// How many result lines a collapsed tool cell shows.
const TOOL_PREVIEW_LINES: usize = 6;

/// One visual block in the conversation.
pub enum Cell {
    /// A user message (submitted prompt or drained steering message).
    User { text: String },
    /// Streaming assistant prose.
    Assistant { text: String },
    /// The turn's FINAL assistant prose, promoted from the trailing
    /// Assistant cell when the run ends - the emphasized counterpart to
    /// the User task that opened the turn. Mid-turn prose between tool
    /// calls stays a plain Assistant cell.
    Answer { text: String },
    /// Streaming assistant thinking (rendered dim).
    Thinking { text: String },
    /// A tool call and (once finished) its result.
    Tool {
        /// Tool call id, used to attach the result when it completes.
        id: String,
        name: String,
        /// Compact JSON of the final arguments (the cell is created on ToolCallEnd).
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

/// One horizontal band of the two-pane view: the conversation cells on the
/// left and the tool calls they triggered on the right. Both panes render
/// a band across the SAME rows - that alignment is what visually assigns a
/// tool call to its reasoning step.
pub struct Step {
    /// Indices into `Transcript::cells` for the conversation pane.
    pub left: Vec<usize>,
    /// Indices into `Transcript::cells` for the tool pane.
    pub right: Vec<usize>,
    /// 1-based number rendered in BOTH panes when the band has tool calls;
    /// `None` when there is nothing to correlate. Restarts at every user
    /// prompt, so numbers stay small and turn-scoped.
    pub marker: Option<usize>,
}

/// The two aligned panes plus the hit-test map, rebuilt every frame.
pub struct Columns {
    pub left: Vec<Line<'static>>,
    pub right: Vec<Line<'static>>,
    /// For each LEFT visual line: which cell it renders. `None` for chrome
    /// (band rules, spacers, alignment padding). A mouse click resolves
    /// through this map.
    pub cell_at: Vec<Option<usize>>,
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

    /// Promote the trailing assistant prose to an Answer cell. Called when
    /// a run ends - only then is "the last text the model wrote" known to
    /// be its final answer; during streaming every Assistant cell might
    /// still be followed by another tool call.
    ///
    /// Walks back over trailing bookkeeping (usage, notices) and stops at
    /// anything substantive: a run that ended in an Error cell keeps its
    /// plain cells - there is no "answer" to celebrate.
    pub fn promote_final_answer(&mut self) {
        for cell in self.cells.iter_mut().rev() {
            match cell {
                Cell::Usage { .. } | Cell::Notice { .. } => {}
                Cell::Assistant { text } => {
                    // take() moves the String out (leaving an empty one
                    // behind) so the cell can be REPLACED without cloning
                    // the text - the old cell is overwritten right after.
                    let text = core::mem::take(text);
                    *cell = Cell::Answer { text };
                    return;
                }
                _ => return,
            }
        }
    }

    /// Group cells into visual bands: tool cells go right, everything else
    /// left. A new band starts at every user prompt (turn boundary) and
    /// whenever conversation output follows tool calls - the model has
    /// moved on to its next step.
    #[must_use]
    pub fn steps(&self) -> Vec<Step> {
        let mut out: Vec<Step> = Vec::new();
        let mut number = 0; // tool-band counter, reset per turn
        for (index, cell) in self.cells.iter().enumerate() {
            let is_tool = matches!(cell, Cell::Tool { .. });
            let is_user = matches!(cell, Cell::User { .. });
            if is_user {
                number = 0;
            }
            let break_band = match out.last() {
                None => true,
                Some(last) => is_user || (!is_tool && !last.right.is_empty()),
            };
            if break_band {
                out.push(Step {
                    left: Vec::new(),
                    right: Vec::new(),
                    marker: None,
                });
            }
            let step = out.last_mut().expect("pushed above");
            if is_tool {
                if step.right.is_empty() {
                    number += 1;
                    step.marker = Some(number);
                }
                step.right.push(index);
            } else {
                step.left.push(index);
            }
        }
        out
    }

    /// Render the two aligned panes for the given inner widths. `selected`
    /// tints that cell's lines so the user sees what Ctrl+O would copy.
    /// Called once per frame; cheap enough at chat-transcript sizes that we
    /// don't cache (ratatui diffs the actual terminal writes anyway).
    #[must_use]
    pub fn to_columns(
        &self,
        left_width: u16,
        right_width: u16,
        selected: Option<usize>,
    ) -> Columns {
        let left_width = left_width.max(10) as usize;
        let right_width = right_width.max(10) as usize;
        let mut columns = Columns {
            left: Vec::new(),
            right: Vec::new(),
            cell_at: Vec::new(),
        };

        for step in self.steps() {
            // A blank spacer between bands, but not at the very top.
            if !columns.left.is_empty() {
                push_chrome(&mut columns, Line::default(), Line::default());
            }
            // The band rule: the SAME number at the SAME row in both panes
            // - the visible thread from a reasoning step to its tool calls
            // even when the two sides have very different heights.
            if let Some(number) = step.marker {
                push_chrome(
                    &mut columns,
                    band_rule(number, left_width),
                    band_rule(number, right_width),
                );
            }

            let mut left: Vec<Line<'static>> = Vec::new();
            let mut cell_at: Vec<Option<usize>> = Vec::new();
            for (position, &index) in step.left.iter().enumerate() {
                if position > 0 {
                    left.push(Line::default());
                    cell_at.push(None);
                }
                let mut lines = conversation_lines(&self.cells[index], left_width);
                if selected == Some(index) {
                    // The line style paints first, spans patch on top: a
                    // bg-only style tints the row without touching the
                    // span foregrounds.
                    for line in &mut lines {
                        line.style = line.style.patch(theme::SELECTED);
                    }
                }
                cell_at.extend(std::iter::repeat_n(Some(index), lines.len()));
                left.append(&mut lines);
            }

            let mut right: Vec<Line<'static>> = Vec::new();
            for (position, &index) in step.right.iter().enumerate() {
                if position > 0 {
                    right.push(Line::default());
                }
                right.extend(tool_lines(&self.cells[index], right_width));
            }

            // Pad the shorter pane so the NEXT band starts at the same row
            // on both sides - this is the whole alignment trick.
            while left.len() < right.len() {
                left.push(Line::default());
                cell_at.push(None);
            }
            while right.len() < left.len() {
                right.push(Line::default());
            }

            columns.left.append(&mut left);
            columns.cell_at.append(&mut cell_at);
            columns.right.append(&mut right);
        }
        columns
    }

    /// The raw text a copy places on the clipboard - the UNRENDERED cell
    /// content (no `> ` prefix, no wrapping, markdown source exactly as
    /// the model wrote it). Tool cells return None: they live in the right
    /// pane and stay out of the copy feature.
    #[must_use]
    pub fn copy_text(&self, index: usize) -> Option<&str> {
        match self.cells.get(index)? {
            Cell::User { text }
            | Cell::Assistant { text }
            | Cell::Answer { text }
            | Cell::Thinking { text }
            | Cell::Error { text }
            | Cell::Notice { text }
            | Cell::Usage { text } => Some(text),
            Cell::Tool { .. } => None,
        }
    }
}

/// Append one chrome line (band rule or spacer) to both panes at once,
/// keeping the three parallel vectors in lockstep.
fn push_chrome(columns: &mut Columns, left: Line<'static>, right: Line<'static>) {
    columns.left.push(left);
    columns.right.push(right);
    columns.cell_at.push(None);
}

/// `─ 3 ────...` filled to `width` columns: the band's number, dim.
fn band_rule(number: usize, width: usize) -> Line<'static> {
    let label = format!("─ {number} ");
    let fill = width.saturating_sub(label.chars().count());
    Line::from(Span::styled(
        format!("{label}{}", "─".repeat(fill)),
        theme::STEP_RULE,
    ))
}

/// Styled, wrapped lines for one CONVERSATION cell - everything except
/// tool calls, which render via [`tool_lines`] into the right pane.
fn conversation_lines(cell: &Cell, width: usize) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::new();
    match cell {
        Cell::User { text } => {
            push_wrapped(&mut out, &format!("> {text}"), width, theme::TASK);
        }
        Cell::Assistant { text } => {
            // Assistant prose is markdown; the base style keeps the cell
            // identity (markdown accents PATCH onto it).
            out.extend(crate::modes::interactive::markdown::render(
                text,
                width,
                theme::ASSISTANT,
            ));
        }
        Cell::Answer { text } => {
            out.extend(crate::modes::interactive::markdown::render(
                text,
                width,
                theme::ANSWER,
            ));
        }
        Cell::Thinking { text } => {
            push_wrapped(&mut out, text, width, theme::REASONING);
        }
        Cell::Error { text } => {
            push_wrapped(&mut out, &format!("error: {text}"), width, theme::ERROR);
        }
        Cell::Notice { text } => {
            push_wrapped(&mut out, text, width, theme::NOTICE);
        }
        Cell::Usage { text } => {
            push_wrapped(&mut out, text, width, theme::DETAIL);
        }
        Cell::Tool { .. } => {}
    }
    out
}

/// Styled, wrapped lines for one TOOL cell: the `[name] args` header, then
/// a bounded result preview - the FULL output already went to the model
/// (and to the trace log); the pane is a digest.
fn tool_lines(cell: &Cell, width: usize) -> Vec<Line<'static>> {
    let Cell::Tool {
        name, args, result, ..
    } = cell
    else {
        return Vec::new();
    };
    let mut out: Vec<Line<'static>> = Vec::new();
    push_wrapped(
        &mut out,
        &format!("[{name}] {args}"),
        width,
        theme::TOOL_HEADER,
    );
    match result {
        None => push_wrapped(&mut out, "  ...", width, theme::DETAIL),
        Some(outcome) => {
            let style = if outcome.is_error {
                theme::ERROR
            } else {
                theme::DETAIL
            };
            let total = outcome.text.lines().count();
            for line in outcome.text.lines().take(TOOL_PREVIEW_LINES) {
                push_wrapped(&mut out, &format!("  {line}"), width, style);
            }
            if total > TOOL_PREVIEW_LINES {
                push_wrapped(
                    &mut out,
                    &format!("  ... ({} more lines)", total - TOOL_PREVIEW_LINES),
                    width,
                    theme::DETAIL,
                );
            }
        }
    }
    out
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

    /// Extract the plain text from a ratatui `Line` (concatenated span content).
    fn line_text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

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

    #[test]
    fn promote_final_answer_targets_trailing_prose_only() {
        let mut t = Transcript::default();
        t.cells.push(Cell::User {
            text: "task".into(),
        });
        t.append_assistant("mid-turn note");
        t.cells.push(Cell::Tool {
            id: "1".into(),
            name: "read".into(),
            args: "{}".into(),
            result: None,
        });
        t.append_assistant("the final answer");
        t.cells.push(Cell::Usage {
            text: "[usage]".into(),
        });

        t.promote_final_answer();

        assert!(
            matches!(t.cells.last(), Some(Cell::Usage { .. })),
            "usage stays last"
        );
        assert!(matches!(&t.cells[3], Cell::Answer { text } if text == "the final answer"));
        assert!(
            matches!(&t.cells[1], Cell::Assistant { text } if text == "mid-turn note"),
            "mid-turn prose must stay plain"
        );
    }

    #[test]
    fn promote_final_answer_skips_error_runs() {
        let mut t = Transcript::default();
        t.append_assistant("half an answer");
        t.cells.push(Cell::Error {
            text: "boom".into(),
        });
        t.promote_final_answer();
        assert!(!t.cells.iter().any(|c| matches!(c, Cell::Answer { .. })));
    }

    /// A bare tool cell for grouping tests.
    fn tool(id: &str) -> Cell {
        Cell::Tool {
            id: id.into(),
            name: "grep".into(),
            args: "{}".into(),
            result: None,
        }
    }

    #[test]
    fn steps_split_tools_right_and_break_on_the_next_thought() {
        let mut t = Transcript::default();
        t.cells.push(Cell::User {
            text: "task".into(),
        });
        t.append_thinking("first thought");
        t.cells.push(tool("1"));
        t.cells.push(tool("2"));
        t.append_thinking("second thought");
        t.cells.push(tool("3"));
        t.append_assistant("done");

        let steps = t.steps();
        assert_eq!(steps.len(), 3);
        // Band 1: prompt + first thought, with the two calls it triggered.
        assert_eq!(steps[0].left, vec![0, 1]);
        assert_eq!(steps[0].right, vec![2, 3]);
        assert_eq!(steps[0].marker, Some(1));
        // Band 2: the thought AFTER tool results starts a fresh band.
        assert_eq!(steps[1].left, vec![4]);
        assert_eq!(steps[1].right, vec![5]);
        assert_eq!(steps[1].marker, Some(2));
        // Band 3: trailing prose, no tools, no number.
        assert_eq!(steps[2].left, vec![6]);
        assert!(steps[2].right.is_empty());
        assert_eq!(steps[2].marker, None);
    }

    #[test]
    fn step_numbers_restart_at_every_user_prompt() {
        let mut t = Transcript::default();
        t.cells.push(Cell::User { text: "one".into() });
        t.cells.push(tool("1"));
        t.cells.push(Cell::User { text: "two".into() });
        t.cells.push(tool("2"));
        let steps = t.steps();
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].marker, Some(1));
        assert_eq!(steps[1].marker, Some(1), "numbering is turn-scoped");
    }

    #[test]
    fn columns_keep_both_panes_the_same_height() {
        let mut t = Transcript::default();
        t.append_thinking("short");
        t.cells.push(Cell::Tool {
            id: "1".into(),
            name: "read".into(),
            args: "{}".into(),
            result: Some(ToolOutcome {
                text: "a\nb\nc\nd".into(),
                is_error: false,
            }),
        });
        t.append_thinking("after");
        let columns = t.to_columns(40, 40, None);

        // The invariant everything else rests on: one shared window can
        // slice both panes.
        assert_eq!(columns.left.len(), columns.right.len());
        assert_eq!(columns.left.len(), columns.cell_at.len());

        // The band rule carries the same number at the same row...
        let left_rule = columns
            .left
            .iter()
            .position(|l| line_text(l).starts_with("─ 1 "));
        let right_rule = columns
            .right
            .iter()
            .position(|l| line_text(l).starts_with("─ 1 "));
        assert!(left_rule.is_some(), "band rule missing on the left");
        assert_eq!(left_rule, right_rule, "rule rows must align");

        // ...and the second thought renders BELOW the tool block: the
        // left pane was padded so the next band starts aligned.
        let after_row = columns
            .left
            .iter()
            .position(|l| line_text(l) == "after")
            .expect("second thought rendered");
        let last_tool_row = columns
            .right
            .iter()
            .rposition(|l| line_text(l).contains('d'))
            .expect("tool preview rendered");
        assert!(after_row > last_tool_row, "alignment padding missing");
    }

    #[test]
    fn cell_map_points_clicks_at_the_right_cell() {
        let mut t = Transcript::default();
        t.append_thinking("short");
        t.cells.push(tool("1"));
        t.append_thinking("after");
        let columns = t.to_columns(40, 40, None);
        assert_eq!(columns.cell_at[0], None, "the band rule is chrome");
        assert_eq!(columns.cell_at[1], Some(0), "the thinking line");
        assert_eq!(
            *columns.cell_at.last().unwrap(),
            Some(2),
            "the second thought"
        );
    }

    #[test]
    fn selected_cell_lines_carry_the_highlight_background() {
        let mut t = Transcript::default();
        t.append_assistant("hello world");
        let selected = t.to_columns(40, 40, Some(0));
        assert_eq!(selected.left[0].style.bg, theme::SELECTED.bg);
        let unselected = t.to_columns(40, 40, None);
        assert_eq!(unselected.left[0].style.bg, None);
    }

    #[test]
    fn copy_text_returns_raw_text_for_conversation_cells_only() {
        let mut t = Transcript::default();
        t.cells.push(Cell::User {
            text: "the task".into(),
        });
        t.cells.push(tool("1"));
        assert_eq!(t.copy_text(0), Some("the task"));
        assert_eq!(t.copy_text(1), None, "tool cells stay out of copy");
        assert_eq!(t.copy_text(9), None, "out of range is a soft None");
    }
}
