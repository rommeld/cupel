//! The transcript render model.
//!
//! Agent events describe *what happened*; the transcript describes *what to
//! draw*. Keeping a separate render model (a `Vec<Cell>`) instead of drawing
//! straight from `AgentMessage`s has two payoffs:
//!
//! 1. Streaming deltas mutate the last cell in place (append to the text
//!    being typed out) instead of re-deriving the whole view per event.
//! 2. UI-only state (tool results attached to their calls, expansion
//!    ) has an obvious home that the agent knows nothing about.
//!
//! The view is ONE column: cells render top to bottom in the order they
//! happened, tool calls inline between the reasoning that triggered them
//! and the prose that follows. [`Transcript::to_lines`] flattens the cells
//! into styled lines plus a line->cell map for mouse hit-testing.

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthChar;

use crate::modes::interactive::theme;

use std::time::{Duration, Instant};

const TOOL_PREVIEW_LINES: usize = 6;

/// One visual block in the conversation.
pub enum Cell {
    User {
        text: String,
    },
    Assistant {
        text: String,
    },
    Answer {
        text: String,
    },
    Thinking {
        text: String,
    },
    Tool {
        id: String,
        name: String,
        call: String,
        expanded: bool,
        started_at: Option<Instant>,
        live: Option<String>,
        result: Option<ToolOutcome>,
    },
    Error {
        text: String,
    },
    Notice {
        text: String,
    },
    Usage {
        text: String,
    },
    Summary {
        text: String,
    },
}

pub struct ToolOutcome {
    pub text: String,
    pub is_error: bool,
    pub diff: Option<String>,
    pub took: Option<Duration>,
}

/// The rendered column plus its hit-test map, rebuilt every frame.
pub struct Rendered {
    pub lines: Vec<Line<'static>>,
    /// For each visual line: which cell it renders. `None` for the blank
    /// spacer between cells. A mouse click resolves through this map.
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

    /// The tool cell for a call id. Searches from the end.
    fn tool_mut(&mut self, tool_call_id: &str) -> Option<&mut Cell> {
        self.cells
            .iter_mut()
            .rev()
            .find(|cell| matches!(cell, Cell::Tool { id, .. } if id == tool_call_id))
    }

    /// The loop started executing a call. Start the clock.
    pub fn mark_tool_started(&mut self, tool_call_id: &str) {
        if let Some(Cell::Tool { started_at, .. }) = self.tool_mut(tool_call_id) {
            *started_at = Some(Instant::now());
        }
    }

    /// A progress snapshot from a running tool.
    pub fn attach_tool_progress(&mut self, tool_call_id: &str, text: String) {
        if let Some(Cell::Tool {
            live, result: None, ..
        }) = self.tool_mut(tool_call_id)
        {
            *live = Some(text);
        }
    }

    /// Attach a finished result to its tool cell (matched by call id). The
    /// cell's clock, if it was started, becomes the outcome's `took`.
    pub fn attach_tool_result(&mut self, tool_call_id: &str, mut outcome: ToolOutcome) {
        if let Some(Cell::Tool {
            started_at,
            live,
            result,
            ..
        }) = self.tool_mut(tool_call_id)
        {
            outcome.took = started_at.map(|started| started.elapsed());
            *live = None;
            *result = Some(outcome);
        }
    }

    /// Flip one toll cell between preview and full result.
    pub fn toggle_tool(&mut self, index: usize) {
        if let Some(Cell::Tool { expanded, .. }) = self.cells.get_mut(index) {
            *expanded = !*expanded;
        }
    }

    /// Expand or collapse every tool cell (Ctrl+T).
    pub fn set_all_tools_expanded(&mut self, expanded: bool) {
        for cell in &mut self.cells {
            if let Cell::Tool { expanded: e, .. } = cell {
                *e = expanded;
            }
        }
    }

    /// Promote the trailing assistant prose to an Answer cell. Called when
    /// a run ends: only then is "the last text the model wrote" known to
    /// be its final answer; during streaming every Assistant cell might
    /// still be followed by another tool call.
    ///
    /// Walks back over trailing bookkeeping (usage, notices) and stops at
    /// anything substantive: a run that ended in an Error cell keeps its
    /// plain cells. There is no "answer" to celebrate.
    pub fn promote_final_answer(&mut self) {
        for cell in self.cells.iter_mut().rev() {
            match cell {
                Cell::Usage { .. } | Cell::Notice { .. } => {}
                Cell::Assistant { text } => {
                    // take() moves the String out (leaving an empty one
                    // behind) so the cell can be replaced without cloning
                    // the text.
                    let text = core::mem::take(text);
                    *cell = Cell::Answer { text };
                    return;
                }
                _ => return,
            }
        }
    }

    /// Flatten every cell into styled, wrapped lines for the given inner
    /// width. `selected` tints that cell's lines so the user sees what
    /// Ctrl+O would copy. Called once per frame; cheap enough at
    /// chat-transcript sizes that we don't cache (ratatui diffs the actual
    /// terminal writes anyway).
    #[must_use]
    pub fn to_lines(&self, width: u16, selected: Option<usize>) -> Rendered {
        let width = width.max(10) as usize;
        let mut rendered = Rendered {
            lines: Vec::new(),
            cell_at: Vec::new(),
        };
        for (index, cell) in self.cells.iter().enumerate() {
            // A blank spacer between cells, but not at the very top.
            if !rendered.lines.is_empty() {
                rendered.lines.push(Line::default());
                rendered.cell_at.push(None);
            }
            let mut lines = cell_lines(cell, width);
            if selected == Some(index) {
                // The line style paints first, spans patch on top: a
                // bg-only style tints the row without touching the
                // span foregrounds.
                for line in &mut lines {
                    line.style = line.style.patch(theme::SELECTED);
                }
            }
            rendered
                .cell_at
                .extend(std::iter::repeat_n(Some(index), lines.len()));
            rendered.lines.append(&mut lines);
        }
        rendered
    }

    /// The raw text a copy places on the clipboard. The unrendered cell
    /// content (no `> ` prefix, no wrapping, markdown source exactly as
    /// the model wrote it). Tool cells return None: a click on them toggles
    /// the preview instead of selecting, so they stay out of the copy
    /// feature.
    #[must_use]
    pub fn copy_text(&self, index: usize) -> Option<&str> {
        match self.cells.get(index)? {
            Cell::User { text }
            | Cell::Assistant { text }
            | Cell::Answer { text }
            | Cell::Thinking { text }
            | Cell::Error { text }
            | Cell::Notice { text }
            | Cell::Usage { text }
            | Cell::Summary { text } => Some(text),
            Cell::Tool { .. } => None,
        }
    }
}

/// Styled, wrapped lines for one cell. Tool cells delegate to
/// [`tool_lines`]; everything else is conversation.
fn cell_lines(cell: &Cell, width: usize) -> Vec<Line<'static>> {
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
        Cell::Summary { text } => {
            push_wrapped(&mut out, "[context summary]", width, theme::NOTICE);
            out.extend(crate::modes::interactive::markdown::render(
                text,
                width,
                theme::REASONING,
            ));
        }
        Cell::Tool { .. } => out.extend(tool_lines(cell, width)),
    }
    out
}

/// Styled, wrapped lines for one tool cell: the call's header line, then
/// the result. The full output already went to the model. The preview is a
/// digest, and the marker line says how to see the rest.
fn tool_lines(cell: &Cell, width: usize) -> Vec<Line<'static>> {
    let Cell::Tool {
        name,
        call,
        expanded,
        started_at,
        live,
        result,
        ..
    } = cell
    else {
        return Vec::new();
    };
    let mut out: Vec<Line<'static>> = Vec::new();
    push_wrapped(&mut out, call, width, theme::TOOL_HEADER);
    match result {
        None => {
            if let Some(live) = live {
                let lines: Vec<&str> = live.lines().collect();
                let hidden = lines.len().saturating_sub(TOOL_PREVIEW_LINES);
                for line in &lines[hidden..] {
                    push_wrapped(&mut out, &format!("  {line}"), width, theme::DETAIL);
                }
            }
            let waiting = match started_at {
                Some(started) => format!("  ... running {}", format_seconds(started.elapsed())),
                None => "  ...".to_string(),
            };
            push_wrapped(&mut out, &waiting, width, theme::DETAIL);
        }
        Some(ToolOutcome {
            diff: Some(diff), ..
        }) => push_diff(&mut out, diff, width),
        Some(outcome) => {
            let style = if outcome.is_error {
                theme::ERROR
            } else {
                theme::DETAIL
            };
            let lines: Vec<&str> = outcome.text.lines().collect();
            let hidden = lines.len().saturating_sub(TOOL_PREVIEW_LINES);
            if *expanded || hidden == 0 {
                for line in &lines {
                    push_wrapped(&mut out, &format!(" {line} "), width, style);
                }
            } else if previews_the_tail(name) {
                push_wrapped(
                    &mut out,
                    &format!("  ... ({hidden} earlier lines, ctrl+t to expand)"),
                    width,
                    theme::DETAIL,
                );
                for line in &lines[hidden..] {
                    push_wrapped(&mut out, &format!("  {line}"), width, style);
                }
            } else {
                for line in &lines[..TOOL_PREVIEW_LINES] {
                    push_wrapped(&mut out, &format!("  {line}"), width, style);
                }
                push_wrapped(
                    &mut out,
                    &format!("  ... ({hidden} more lines, ctrl+t to expand)"),
                    width,
                    theme::DETAIL,
                );
            }
            if let Some(took) = outcome.took
                && previews_the_tail(name)
            {
                push_wrapped(
                    &mut out,
                    &format!("  took {}", format_seconds(took)),
                    width,
                    theme::DETAIL,
                );
            }
        }
    }
    out
}

fn format_seconds(duration: Duration) -> String {
    format!("{:.1}s", duration.as_secs_f64())
}

/// Which tool's output is read from the end. Only the shell_ a build or
/// test run buries its verdict under hundreds of progress lines.
fn previews_the_tail(tool_name: &str) -> bool {
    tool_name == "bash"
}

/// Diff lines, styled by their first byte. The patch tool's diff format
/// (text_diff.rs) puts the marker first.
/// `-12 old`, `+12 new`, `12 context`, and a `   ...` row between hunks.
fn push_diff(out: &mut Vec<Line<'static>>, diff: &str, width: usize) {
    for line in diff.lines() {
        let style = match line.as_bytes().first() {
            Some(b'+') => theme::DIFF_ADD,
            Some(b'-') => theme::DIFF_DEL,
            _ => theme::DIFF_CTX,
        };
        push_wrapped(out, &format!("  {line}"), width, style);
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

/// Split `"foo bar  baz"` into `["foo ", "bar  ", "baz"]` words own their
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
            call: "{}".into(),
            expanded: false,
            started_at: None,
            live: None,
            result: None,
        });
        transcript.attach_tool_result(
            "call_1",
            ToolOutcome {
                text: "hit".into(),
                is_error: false,
                diff: None,
                took: None,
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

    /// A finished bash cell with ten numbered output lines.
    fn bash_cell(expanded: bool) -> Cell {
        Cell::Tool {
            id: "1".into(),
            name: "bash".into(),
            call: "$ cargo test".into(),
            expanded,
            started_at: None,
            live: None,
            result: Some(ToolOutcome {
                text: (1..=10)
                    .map(|i| format!("line {i}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
                is_error: false,
                diff: None,
                took: None,
            }),
        }
    }

    #[test]
    fn bash_previews_the_tail_and_files_the_head() {
        let texts: Vec<String> = tool_lines(&bash_cell(false), 60)
            .iter()
            .map(line_text)
            .collect();
        assert_eq!(texts[0], "$ cargo test");
        assert_eq!(texts[1], "  ... (4 earlier lines, ctrl+t to expand)");
        assert_eq!(texts[2], "  line 5");
        assert_eq!(texts.last().unwrap(), "  line 10");

        let mut read = bash_cell(false);
        if let Cell::Tool { name, .. } = &mut read {
            *name = "read".into();
        }
        let texts: Vec<String> = tool_lines(&read, 60).iter().map(line_text).collect();
        assert_eq!(texts[1], "  line 1");
        assert_eq!(texts[6], "  line 6");
        assert_eq!(texts[7], "  ... (4 more lines, ctrl+t to expand)");
    }

    #[test]
    fn a_running_tool_shows_its_latest_output_and_a_clock() {
        let mut t = Transcript::default();
        let mut cell = bash_cell(false);
        if let Cell::Tool { result, .. } = &mut cell {
            *result = None;
        }
        t.cells.push(cell);
        t.mark_tool_started("1");
        t.attach_tool_progress("1", "a\nb\nc\nd\ne\nf\ng".into());

        let texts: Vec<String> = tool_lines(&t.cells[0], 60).iter().map(line_text).collect();
        assert_eq!(texts[1], "  b", "the snapshot's tail, six lines");
        assert_eq!(texts[6], "  g");
        assert!(texts[7].starts_with("  ... running "), "{texts:?}");

        t.attach_tool_result(
            "1",
            ToolOutcome {
                text: "g".into(),
                is_error: false,
                diff: None,
                took: None,
            },
        );
        let texts: Vec<String> = tool_lines(&t.cells[0], 60).iter().map(line_text).collect();
        assert_eq!(texts, vec!["$ cargo test", " g ", "  took 0.0s"]);
        assert!(matches!(&t.cells[0], Cell::Tool { live: None, .. }));
    }

    #[test]
    fn expansion_shows_every_line_and_toggles_per_cell_or_for_all() {
        let texts: Vec<String> = tool_lines(&bash_cell(true), 60)
            .iter()
            .map(line_text)
            .collect();
        assert_eq!(texts.len(), 11, "header + all ten lines");
        assert!(!texts.iter().any(|t| t.contains("expand")));

        let mut t = Transcript::default();
        t.cells.push(bash_cell(false));
        t.cells.push(bash_cell(false));
        t.toggle_tool(1);
        assert!(matches!(
            t.cells[0],
            Cell::Tool {
                expanded: false,
                ..
            }
        ));
        assert!(matches!(t.cells[1], Cell::Tool { expanded: true, .. }));
        t.set_all_tools_expanded(true);
        assert!(matches!(t.cells[0], Cell::Tool { expanded: true, .. }));
        t.set_all_tools_expanded(false);
        assert!(matches!(
            t.cells[1],
            Cell::Tool {
                expanded: false,
                ..
            }
        ));
    }

    #[test]
    fn diff_replaces_result_text_and_colors_by_marker() {
        let cell = Cell::Tool {
            id: "1".into(),
            name: "apply_patch".into(),
            call: "{}".into(),
            expanded: false,
            started_at: None,
            live: None,
            result: Some(ToolOutcome {
                text: "Success. Updated the following files:\nM a.rs".into(),
                is_error: false,
                diff: Some(" 1 fn main() {\n-2     old();\n+2     new();\n 3 }".into()),
                took: None,
            }),
        };
        let lines = tool_lines(&cell, 40);
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        assert_eq!(texts[0], "{}");
        assert_eq!(texts[1], "   1 fn main() {");
        assert_eq!(texts[2], "  -2     old();");
        assert_eq!(texts[3], "  +2     new();");
        assert_eq!(texts[4], "   3 }");
        assert_eq!(lines[1].spans[0].style, theme::DIFF_CTX);
        assert_eq!(lines[2].spans[0].style, theme::DIFF_DEL);
        assert_eq!(lines[3].spans[0].style, theme::DIFF_ADD);
        assert_eq!(lines[4].spans[0].style, theme::DIFF_CTX);
        assert!(
            !texts.iter().any(|t| t.contains("Updated the following")),
            "the confirmation line is for the model, not the screen"
        );
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
            call: "{}".into(),
            expanded: false,
            started_at: None,
            live: None,
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

    fn tool(id: &str) -> Cell {
        Cell::Tool {
            id: id.into(),
            name: "grep".into(),
            call: "{}".into(),
            expanded: false,
            started_at: None,
            live: None,
            result: None,
        }
    }

    #[test]
    fn tools_render_inline_in_event_order() {
        let mut t = Transcript::default();
        t.append_thinking("let me look");
        t.cells.push(tool("1"));
        t.append_assistant("found it");
        let texts: Vec<String> = t.to_lines(40, None).lines.iter().map(line_text).collect();
        // One column, a blank spacer between cells, no band rule anywhere.
        assert_eq!(
            texts,
            vec!["let me look", "", "{}", "  ...", "", "found it"]
        );
    }

    #[test]
    fn cell_map_points_clicks_at_the_right_cell() {
        let mut t = Transcript::default();
        t.append_thinking("short");
        t.cells.push(tool("1"));
        t.append_thinking("after");
        let rendered = t.to_lines(40, None);
        assert_eq!(rendered.cell_at.len(), rendered.lines.len());
        assert_eq!(rendered.cell_at[0], Some(0), "the thinking line");
        assert_eq!(rendered.cell_at[1], None, "the spacer is chrome");
        assert_eq!(rendered.cell_at[2], Some(1), "the tool's header line");
        assert_eq!(
            *rendered.cell_at.last().unwrap(),
            Some(2),
            "the second thought"
        );
    }

    #[test]
    fn selected_cell_lines_carry_the_highlight_background() {
        let mut t = Transcript::default();
        t.append_assistant("hello world");
        let selected = t.to_lines(40, Some(0));
        assert_eq!(selected.lines[0].style.bg, theme::SELECTED.bg);
        let unselected = t.to_lines(40, None);
        assert_eq!(unselected.lines[0].style.bg, None);
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
