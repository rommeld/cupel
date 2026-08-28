//! The TUI palette - every style the transcript and chrome use, in ONE
//! place.
//!
//! As a module, the visual hierarchy of turn is a single reviewable
//! unit:
//!
//! - TASK and ANSWER are the emphasized endpoints of a turn (what was
//! asked, what came out).
//! - REASONING and TOOL traffic are the de-emphasized middle,
//! - errors/notices keep their conventional terminal colors.
//!
//! These are true `const`s: ratatui`s `Style::new()`, `fg`, `bg`, and
//! `add_modifier` are const fns. Note the chained `add_modifier` calls
//! where two modifiers combin - the `|` operator (BitOr) is NOT a const
//! fn, so `Modifier::DIM | Modifier::ITALIC` would not compile here.

use ratatui::style::{Color, Modifier, Style};

/// The user's task opening a turn: bright and bold, the "> " prefix rides
/// in transcript.rs.
pub const TASK: Style = Style::new().fg(Color::LightGreen);
/// Mid-turn assistant prose (commentary between tool calls): plain.
pub const ASSISTANT: Style = Style::new();
/// The turn's final answer: the emphasized couterpart to TASK. Magenta
/// because green (task), cyan (tools), red (errors), and yellow
/// (notices) are taken - and BOLD alone is too subtle next to plain
/// prose.
pub const ANSWER: Style = Style::new().fg(Color::Magenta);
/// Model reasoning: present but visually receded (M3 tunes this).
pub const REASONING: Style = Style::new()
    .fg(Color::DarkGray)
    .add_modifier(Modifier::ITALIC);
/// A tool call header (`[name] {args}`).
pub const TOOL_HEADER: Style = Style::new().fg(Color::Cyan);
/// De-emphasized detail lines: pending markers, ok tool output, overflow
/// notes, usage summaries.
pub const DETAIL: Style = Style::new();
/// Errors and failed tool results.
pub const ERROR: Style = Style::new().fg(Color::Red);
/// Status notices (retry, compaction, /provider listings).
pub const NOTICE: Style = Style::new().fg(Color::Yellow);

/// Input border while a run is active / while idle.
pub const INPUT_BORDER_BUSY: Style = Style::new().fg(Color::Yellow);
pub const INPUT_BORDER_IDLE: Style = Style::new().fg(Color::DarkGray);
/// Box titles and the footer line.
pub const CHROME: Style = Style::new().add_modifier(Modifier::DIM);
/// The " ↓ N more " overlay while scrolled up.
pub const SCROLL_MARKER: Style = Style::new().fg(Color::Black).bg(Color::Yellow);
/// Borders and titles of the two transcript panes (conversation | tools).
pub const PANE_BORDER: Style = Style::new().fg(Color::DarkGray);
/// The numbered band rule that ties a reasoning step (left pane) to the
/// tool calls it triggered (right pane): same number, same row, both panes.
pub const STEP_RULE: Style = Style::new().fg(Color::DarkGray);
/// Background of the click-selected conversation block (Ctrl+O copies it).
/// bg-only on purpose: a Line's own style paints UNDER its spans, so the
/// highlight tints the row while every span keeps its foreground color.
pub const SELECTED: Style = Style::new().bg(Color::Indexed(237));
/// The scrollbar thumb; the track reuses the pane border color.
pub const SCROLLBAR_THUMB: Style = Style::new().fg(Color::DarkGray);
/// Autocomplete popup rows: the selected one inverts, the rest match the
/// tool color.
pub const POPUP_SELECTED: Style = Style::new().add_modifier(Modifier::REVERSED);
pub const POPUP_ROW: Style = Style::new().fg(Color::Cyan);

// PATCH styles: applied onto a cell's base stayle via Style::patch -
// set fields win, unset fields keep the base. A heading in an Answer cell
// is therefore magenta; only styles that DO set a color (code, links)
// deliberately break out of the cell color, because code is code no matter
// which cell it is in.

/// H1/H2: bold + underlined (H3-H6 get MD_BOLD only - depth fades).
pub const MD_HEADING: Style = Style::new()
    .add_modifier(Modifier::BOLD)
    .add_modifier(Modifier::UNDERLINED);
pub const MD_BOLD: Style = Style::new().add_modifier(Modifier::BOLD);
pub const MD_ITALIC: Style = Style::new().add_modifier(Modifier::ITALIC);
pub const MD_STRIKE: Style = Style::new().add_modifier(Modifier::CROSSED_OUT);
/// Inline code: cupel's cyan accent family (tools, popups)
pub const MD_CODE: Style = Style::new().fg(Color::Cyan);
/// Fenced code blocks: a full-width panel on xtrem-256 index 235
/// (#262626) - subtler and more portable than truecolor, and visually
/// "a surface", not a color.
pub const MD_CODE_BLOCK_BG: Color = Color::Indexed(235);
/// Blockquotes: receded like reasoning, but inside the cell's color.
pub const MD_QUOTE: Style = Style::new()
    .add_modifier(Modifier::DIM)
    .add_modifier(Modifier::ITALIC);
/// List bullets and ordered-list numbers.
pub const MD_BULLET: Style = Style::new().fg(Color::Cyan);
/// Link text: the universal terminal convention.
pub const MD_LINK: Style = Style::new()
    .fg(Color::Blue)
    .add_modifier(Modifier::UNDERLINED);
/// The "(url)" suffix behind a link whose text differs from its target.
pub const MD_LINK_URL: Style = Style::new().add_modifier(Modifier::DIM);
/// Table separators and horizontal rules.
pub const MD_RULE: Style = Style::new().add_modifier(Modifier::DIM);
/// Table header row.
pub const MD_TABLE_HEADER: Style = Style::new().add_modifier(Modifier::BOLD);
