//! Markdown rendering for assistant prose - a hand-rolled SUBSET on
//! ratatui's primitives (Span -> Line), no parser crate.
//!
//! ratatui ships no markdown widget - its docs model text as Span/
//! Line/Style and leave the translation to the app. Ecosystem
//! renderers exist, but LLM output uses small, regular slice of
//! markdown.
//!
//! Styling model: every asccent is a PATCH onto the cell's base style
//! (Style::patch - set fields win, unset fields inherit), so plain text
//! renders byte-identical to the pre-markdown path and an Answer cell
//! keeps its magenta identity under bold/italic. Streaming: to_lines
//! re-renders every frame, so a not-yet-closed `**`or ``` simply styles
//! the rest of the line/cell until the closing delimiter arrives - the
//! next delta self-corrects it.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthChar;

use crate::modes::interactive::theme;
use crate::modes::interactive::transcript::wrap_line;

/// Render markdown into wrapped, styled lines for one transcrip cell.
/// `base` is the cell's identity style (ASSISTANT, ANSWER).
#[must_use]
pub fn render(text: &str, width: usize, base: Style) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let lines: Vec<&str> = text.split('\n').collect();
    let mut in_code = false;
    let mut i = 0;
    while i < lines.len() {
        let trimmed = lines[i].trim_start();
        // Fences toggle code mode; the fence line itself is chrome and
        // not shown (the panel background marks the block).
        if trimmed.starts_with("```") {
            in_code = !in_code;
            i += 1;
            continue;
        }
        if in_code {
            push_code_line(&mut out, lines[i], width);
            i += 1;
            continue;
        }
        // Tables needs lookahead (a pipe row is only a table when the NEXT
        // line is a separator row), so they are handled here where the
        // slice is available - everything else is per-line.
        if trimmed.starts_with('|')
            && is_table_separator(lines.get(i + 1).copied().unwrap_or("default"))
        {
            i += render_table(&mut out, &lines[i..], width, base);
            continue;
        }
        render_block_line(&mut out, lines[i], width, base);
        i += 1;
    }
    if out.is_empty() {
        out.push(Line::default());
    }
    out
}

/// The effective style for the current flag state - computed at flush
/// time, so nested emphasis composes via path stacking.
fn inline_style(base: Style, bold: bool, italic: bool, strike: bool) -> Style {
    let mut style = base;
    if bold {
        style = style.patch(theme::MD_BOLD);
    }
    if italic {
        style = style.patch(theme::MD_ITALIC);
    }
    if strike {
        style = style.patch(theme::MD_STRIKE);
    }
    style
}

/// Close the pending run of plain characters into a segment.
fn flush(segements: &mut Vec<(String, Style)>, current: &mut String, style: Style) {
    if !current.is_empty() {
        segements.push((core::mem::take(current), style));
    }
}

/// Split one line of markdown into styled segments. Delimiters toggle
/// flags; the text between two flushes shares one style. Unclosed
/// delimiters style the rest of the line - correct for streaming, where
/// the closing half may simply not have arrived yet.
fn inline_spans(text: &str, base: Style) -> Vec<(String, Style)> {
    let chars: Vec<char> = text.chars().collect();
    let mut segments: Vec<(String, Style)> = Vec::new();
    let mut current = String::new();
    let (mut bold, mut italic, mut strike, mut code) = (false, false, false, false);
    let mut i = 0;

    while i < chars.len() {
        let c = chars[i];
        // Inside inline code NOTHING is special except the closing tick.
        if code {
            if c == '`' {
                flush(&mut segments, &mut current, base.patch(theme::MD_CODE));
                code = false;
            } else {
                current.push(c);
            }
            i += 1;
            continue;
        }
        let style = inline_style(base, bold, italic, strike);
        match c {
            // Backslash escapes the next punctuation char.
            '\\' if chars.get(i + 1).is_some_and(char::is_ascii_punctuation) => {
                current.push(chars[i + 1]);
                i += 2;
            }
            '`' => {
                flush(&mut segments, &mut current, style);
                code = true;
                i += 1;
            }
            '~' if chars.get(i + 1) == Some(&'~') => {
                flush(&mut segments, &mut current, style);
                strike = !strike;
                i += 2;
            }
            '*' | '_' => {
                if chars.get(i + 1) == Some(&c) {
                    flush(&mut segments, &mut current, style);
                    bold = !bold;
                    i += 2;
                } else {
                    // Underscores never toggle INSIDE a word (CommonMark's
                    // intraword rule) - snake_style stays literal
                    let intraword = c == '_'
                        && i > 0
                        && chars[i - 1].is_alphanumeric()
                        && chars.get(i + 1).is_some_and(|n| n.is_alphanumeric());
                    if intraword {
                        current.push(c);
                        i += 1;
                        continue;
                    }
                    // Simplified flanking rule: a single marker OPENS only
                    // before a non-space and CLOSES only after one - so
                    // "a*b" and snake_case stay literal.
                    let can_open = chars.get(i + 1).is_some_and(|n| !n.is_whitespace());
                    let can_close = i > 0 && !chars[i - 1].is_whitespace();
                    if (!italic && can_open) || (italic && can_close) {
                        flush(&mut segments, &mut current, style);
                        italic = !italic;
                    } else {
                        current.push(c);
                    }
                    i += 1;
                }
            }
            '[' => match parse_link(&chars[i..]) {
                Some((label, url, consumed)) => {
                    flush(&mut segments, &mut current, style);
                    segments.push((label.clone(), base.patch(theme::MD_LINK)));
                    if !url.is_empty() && url != label {
                        segments.push((format!(" ({url})"), base.patch(theme::MD_LINK_URL)));
                    }
                    i += consumed;
                }
                None => {
                    current.push(']');
                    i += 1;
                }
            },
            _ => {
                current.push(c);
                i += 1;
            }
        }
    }
    // An unclosed code span keeps the code style - see the module doc.
    let final_style = if code {
        base.patch(theme::MD_CODE)
    } else {
        inline_style(base, bold, italic, strike)
    };
    flush(&mut segments, &mut current, final_style);
    if segments.is_empty() {
        segments.push((String::new(), base));
    }
    segments
}

/// `[label(url)]`starting at `chars[0] == '['`. Returns (label, url,
/// consumed chars). Subset rules: no nesting, no escaped brackets, the
/// label renders literally (no emphasis inside linke).
fn parse_link(chars: &[char]) -> Option<(String, String, usize)> {
    let close = chars.iter().position(|c| *c == ']')?;
    if chars.get(close + 1) != Some(&'(') {
        return None;
    }
    let end = chars[close + 2..].iter().position(|c| *c == ')')? + close + 2;
    let label: String = chars[1..close].iter().collect();
    let url: String = chars[close + 2..end].iter().collect();
    Some((label, url, end + 1))
}

/// Word-wrap styled segments to `width`columns and append the lines.
/// `first_prefix`(a bullet, a quote bar) leas the first line; wrapped
/// continuation lines get `cont_indent`spaces of hanging indent. Same
/// algorithm as transcript::warp_line, lifted onto styled characters.
fn push_wrapped_spans(
    out: &mut Vec<Line<'static>>,
    segements: Vec<(String, Style)>,
    width: usize,
    first_prefix: Option<(String, Style)>,
    cont_indent: usize,
) {
    // 1) Flatten to (char, style): the unit that survives both style
    // boundaries and word boundaries.
    let flat: Vec<(char, Style)> = segements
        .iter()
        .flat_map(|(text, style)| text.chars().map(move |c| (c, *style)))
        .collect();

    // 2) Word own their trailing spaces (split_keeping_spaces' rule):
    // a word ends where a space run meets the next non-space.
    let mut words: Vec<&[(char, Style)]> = Vec::new();
    let mut start = 0;
    let mut in_space = false;
    for (idx, (c, _)) in flat.iter().enumerate() {
        if *c == ' ' {
            in_space = true;
        } else if in_space {
            words.push(&flat[start..idx]);
            start = idx;
            in_space = false;
        }
    }
    words.push(&flat[start..]);

    // 3) Greedy fill, exactly like wrap_line - the only new twist is the
    // prefix on line one and the hanging indent afterwards.
    let prefix_width = first_prefix
        .as_ref()
        .map(|(text, _)| display_width(text))
        .unwrap_or(0);
    let mut line: Vec<(char, Style)> = Vec::new();
    let mut line_width = prefix_width;
    let mut first = true;
    let emit = |line: &mut Vec<(char, Style)>, first: &mut bool, out: &mut Vec<Line<'static>>| {
        let mut spans: Vec<Span<'static>> = Vec::new();
        if *first {
            if let Some((text, style)) = &first_prefix {
                spans.push(Span::styled(text.clone(), *style));
            }
            *first = false;
        } else if cont_indent > 0 {
            spans.push(Span::raw(" ".repeat(cont_indent)));
        }
        spans.extend(group_runs(line));
        line.clear();
        out.push(Line::from(spans));
    };

    for word in words {
        let word_width: usize = word.iter().map(|(c, _)| c.width().unwrap_or(0)).sum();
        if line_width + word_width <= width {
            line.extend_from_slice(word);
            line_width += word_width;
            continue;
        }
        if !line.is_empty() || first {
            emit(&mut line, &mut first, out);
            line_width = cont_indent;
        }
        if word_width > width.saturating_sub(cont_indent) {
            // Hard-split on over-long word (URLs, long identifiers).
            for (c, style) in word {
                let w = c.width().unwrap_or(0);
                if line_width + w > width && !line.is_empty() {
                    emit(&mut line, &mut first, out);
                    line_width = cont_indent;
                }
                line.push((*c, *style));
                line_width += w;
            }
        } else {
            line.extend_from_slice(word);
            line_width += word_width;
        }
    }
    emit(&mut line, &mut first, out);
}

/// Group consecutive same-style chars back into spans - the inverse of
/// the flatten in step 1, done last so wrapping never has to think about
/// span boundaries.
fn group_runs(chars: &[(char, Style)]) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut run = String::new();
    let mut run_style: Option<Style> = None;
    for (c, style) in chars {
        if run_style != Some(*style) {
            if let Some(style) = run_style
                && !run.is_empty()
            {
                spans.push(Span::styled(core::mem::take(&mut run), style));
            }
            run_style = Some(*style);
        }
        run.push(*c);
    }
    if let Some(style) = run_style
        && !run.is_empty()
    {
        spans.push(Span::styled(run, style));
    }
    spans
}

/// Display columns of a string (CJK-aware) - the same math wrap_line uses.
fn display_width(text: &str) -> usize {
    text.chars().map(|c| c.width().unwrap_or(0)).sum()
}

fn render_block_line(out: &mut Vec<Line<'static>>, line: &str, width: usize, base: Style) {
    let trimmed = line.trim_start();
    let indent = line.len() - trimmed.len();

    if line.trim().is_empty() {
        out.push(Line::default());
        return;
    }

    // ATX headings: 1-6 hashes plus a space (CommonMark requires the
    // space, which keeps "#hashtag" literal). The hashes are chrome and
    // are dropped; depth maps to empasis (underline fades at H3+).
    let hashes = trimmed.chars().take_while(|c| *c == '#').count();
    if (1..=6).contains(&hashes) && trimmed[hashes..].starts_with(' ') {
        let heading = if hashes <= 2 {
            base.patch(theme::MD_HEADING)
        } else {
            base.patch(theme::MD_BOLD)
        };
        push_wrapped_spans(
            out,
            inline_spans(trimmed[hashes + 1..].trim_start(), heading),
            width,
            None,
            0,
        );
        return;
    }

    // Horizontal rule: three or more of the SAME marker char, alone.
    let rule_char = trimmed.chars().next().unwrap_or(' ');
    if matches!(rule_char, '-' | '*' | '_')
        && trimmed.len() >= 3
        && trimmed.chars().all(|c| c == rule_char)
    {
        out.push(Line::from(Span::styled(
            "─".repeat(width),
            base.patch(theme::MD_RULE),
        )));
        return;
    }

    // Blockqoute: one level (nested '>' collapse into it) - a receded
    // bar-prefixed run, inline markup still active inside.
    if let Some(rest) = trimmed.strip_prefix('>') {
        let quote = base.patch(theme::MD_QUOTE);
        let content = rest.trim_start_matches('>').trim_start();
        push_wrapped_spans(
            out,
            inline_spans(content, quote),
            width,
            Some(("▌ ".to_string(), quote)),
            2,
        );
        return;
    }

    // List items: bullet or "N." marker; nesting via 2-space indent.
    if let Some((marker, rest)) = split_list_marker(trimmed) {
        let level = (indent / 2).min(4);
        let prefix = format!("{}{marker} ", "  ".repeat(level));
        let hang = display_width(&prefix);
        push_wrapped_spans(
            out,
            inline_spans(rest, base),
            width,
            Some((prefix, base.patch(theme::MD_BULLET))),
            hang,
        );
        return;
    }

    push_wrapped_spans(out, inline_spans(line, base), width, None, 0);
}

/// "- "/"* "/"+ " -> a bullet; "12. " -> the number. None = not a list.
fn split_list_marker(text: &str) -> Option<(String, &str)> {
    for bullet in ["- ", "* ", "+ "] {
        if let Some(rest) = text.strip_prefix(bullet) {
            return Some(("•".to_string(), rest));
        }
    }
    let digits = text.chars().take_while(char::is_ascii_digit).count();
    if digits > 0 && digits <= 3 && text[digits..].starts_with(". ") {
        return Some((text[..=digits].to_string(), &text[digits + 2..]));
    }
    None
}

/// One line inside a fenced block: monospace is inherent in a terminal,
/// so the "code look" is a full-width PANEL - every wrapped chunk is
/// padded to the terminal width so the background forms one surface
/// (a bg colors only the cells under its own characters).
fn push_code_line(out: &mut Vec<Line<'static>>, line: &str, width: usize) {
    // Deliberately NOT the cell's base style: code is code, in every
    // cell. Terminal-default fg on the indexed panel tint.
    let style = Style::new().bg(theme::MD_CODE_BLOCK_BG);
    // Tabs render as untrackable-width glyphs; normalize first.
    let line = line.replace('\t', "    ");
    for chunk in wrap_line(&format!(" {line}"), width.saturating_sub(1)) {
        let pad = width.saturating_sub(display_width(&chunk));
        out.push(Line::from(Span::styled(
            format!("{chunk}{}", " ".repeat(pad)),
            style,
        )));
    }
}

/// A separator row: pipes, dashes, colons, spaces - and at least one dash.
fn is_table_separator(line: &str) -> bool {
    let t = line.trim();
    t.starts_with('|') && t.contains('-') && t.chars().all(|c| matches!(c, '|' | '-' | ':' | ' '))
}

/// Cells of one pipe row: outer pipes are chrome, inner pipes split.
/// (Escaped \| is outside the subset - noted in the module doc.)
fn split_cless(row: &str) -> Vec<String> {
    let t = row.trim();
    let t = t.strip_prefix('|').unwrap_or(t);
    let t = t.strip_suffix('|').unwrap_or(t);
    t.split('|').map(|cell| cell.trim().to_string()).collect()
}

/// Render the table starting at lines[0] (header row; lines[1] is the
/// separator). Returns how many source lines were consumed, so the
/// caller's cursor can jump past the whole block.
fn render_table(out: &mut Vec<Line<'static>>, lines: &[&str], width: usize, base: Style) -> usize {
    let mut rows: Vec<Vec<Vec<(String, Style)>>> = Vec::new();
    let mut consumed = 0;
    for (idx, line) in lines.iter().enumerate() {
        if !line.trim().starts_with('|') {
            break;
        }
        consumed = idx + 1;
        if idx == 1 {
            continue;
        }
        let cell_base = if idx == 0 {
            base.patch(theme::MD_TABLE_HEADER)
        } else {
            base
        };
        rows.push(
            split_cless(line)
                .iter()
                .map(|cell| inline_spans(cell, cell_base))
                .collect(),
        );
    }
    let columns = rows.iter().map(Vec::len).max().unwrap_or(0);
    if columns == 0 {
        return consumed.max(1);
    }
    let mut widths = vec![1_usize; columns];
    for row in &rows {
        for (i, cell) in row.iter().enumerate() {
            widths[i] = widths[i].max(segments_width(cell));
        }
    }

    // Total = columns + " │ " joints. Shave the widest column one column
    // at a time unitl it fits (or nothing reasonable is left to shave);
    // over-narrow cells get clipped with an ellipsis below.
    let joints = 3 * (columns - 1);
    let mut total: usize = widths.iter().sum::<usize>() + joints;
    while total > width {
        let widest = (0..columns).max_by_key(|i| widths[*i]).unwrap_or(0);
        if widths[widest] <= 5 {
            break;
        }
        widths[widest] -= 1;
        total -= 1;
    }

    for (idx, row) in rows.iter().enumerate() {
        let mut spans: Vec<Span<'static>> = Vec::new();
        for (i, col_width) in widths.iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled(" │ ".to_string(), base.patch(theme::MD_RULE)));
            }
            let empty = Vec::new();
            let cell = row.get(i).unwrap_or(&empty);
            spans.extend(clip_segments(cell, *col_width));
        }
        out.push(Line::from(spans));
        if idx == 0 {
            // The header underline, aligned with the joints.
            let rule: Vec<String> = widths.iter().map(|w| "─".repeat(*w)).collect();
            out.push(Line::from(Span::styled(
                rule.join("─┼─"),
                base.patch(theme::MD_RULE),
            )));
        }
    }
    consumed.max(1)
}

fn segments_width(segments: &[(String, Style)]) -> usize {
    segments.iter().map(|(text, _)| display_width(text)).sum()
}

/// Emit a cell's segments clipped to `budget`columns (ellipsis when it
/// does not fit) and padded to exactly `budget`- table columns must
/// stay aligned no matter what the cell contains.
fn clip_segments(segments: &[(String, Style)], budget: usize) -> Vec<Span<'static>> {
    let full = segments_width(segments);
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut used = 0;
    for (text, style) in segments {
        let mut piece = String::new();
        for c in text.chars() {
            let w = c.width().unwrap_or(0);
            let limit = if full > budget {
                budget.saturating_sub(1)
            } else {
                budget
            };
            if used + w > limit {
                break;
            }
            used += w;
            piece.push(c);
        }
        if !piece.is_empty() {
            spans.push(Span::styled(piece, *style));
        }
    }
    if full > budget {
        spans.push(Span::styled(
            "…".to_string(),
            Style::new().add_modifier(Modifier::DIM),
        ));
        used += 1;
    }
    if used < budget {
        spans.push(Span::raw(" ".repeat(budget - used)));
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::{Color, Modifier};

    fn flat_text(lines: &[Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    /// The style of the span containing `needle` - panics when absent.
    fn style_of(lines: &[Line<'_>], needle: &str) -> Style {
        lines
            .iter()
            .flat_map(|l| &l.spans)
            .find(|s| s.content.contains(needle))
            .unwrap_or_else(|| panic!("{needle:?} not rendered"))
            .style
    }

    #[test]
    fn plain_text_renders_exactly_like_before() {
        // THE invariant: no markdown syntax = one span, base style,
        // identical wrapping - the renderer is a strict superset.
        let base = Style::new().fg(Color::Magenta);
        let lines = render("hello brave new world", 11, base);
        assert_eq!(flat_text(&lines), ["hello ", "brave new ", "world"]);
        for line in &lines {
            assert_eq!(line.spans.len(), 1);
            assert_eq!(line.spans[0].style, base);
        }
    }

    #[test]
    fn inline_emphasis_patches_the_base_style() {
        let base = Style::new().fg(Color::Magenta);
        let lines = render("a **bold** and *italic* and `code` and ~~gone~~", 80, base);
        let bold = style_of(&lines, "bold");
        assert!(bold.add_modifier.contains(Modifier::BOLD));
        assert_eq!(bold.fg, Some(Color::Magenta), "patch keeps the cell color");
        assert!(
            style_of(&lines, "italic")
                .add_modifier
                .contains(Modifier::ITALIC)
        );
        assert_eq!(style_of(&lines, "code").fg, Some(Color::Cyan));
        assert!(
            style_of(&lines, "gone")
                .add_modifier
                .contains(Modifier::CROSSED_OUT)
        );
        // The delimiters themselves are consumed.
        assert!(!flat_text(&lines).concat().contains("**"));
    }

    #[test]
    fn literals_survive_the_simplified_rules() {
        let text = flat_text(&render(
            r"2 * 3 = 6, snake_case_name, \*literal\*",
            80,
            Style::new(),
        ))
        .concat();
        assert!(text.contains("2 * 3 = 6"), "{text}");
        assert!(text.contains("snake_case_name"), "{text}");
        assert!(text.contains("*literal*"), "{text}");
    }

    #[test]
    fn links_show_label_and_divergent_url() {
        let lines = render(
            "[docs](https://ratatui.rs) and [src/main.rs](src/main.rs)",
            80,
            Style::new(),
        );
        let text = flat_text(&lines).concat();
        assert!(text.contains("docs (https://ratatui.rs)"), "{text}");
        assert!(
            style_of(&lines, "docs")
                .add_modifier
                .contains(Modifier::UNDERLINED)
        );
        // Same label and target: no noisy duplicate.
        assert!(!text.contains("src/main.rs (src/main.rs)"), "{text}");
    }

    #[test]
    fn a_word_crossing_a_style_boundary_never_splits() {
        // "**bo**ld" is ONE word of two styled halves; at width 6 it must
        // wrap as a unit, not break between "bo" and "ld".
        let lines = render("xxxx **bo**ld", 6, Style::new());
        let text = flat_text(&lines);
        assert_eq!(text, ["xxxx ", "bold"], "{text:?}");
    }

    #[test]
    fn unclosed_streaming_delimiters_style_the_tail() {
        // Mid-stream a closing ** may not have arrived yet - the tail
        // renders bold and self-corrects on the next delta.
        let lines = render("a **stream", 80, Style::new());
        assert!(
            style_of(&lines, "stream")
                .add_modifier
                .contains(Modifier::BOLD)
        );
    }

    #[test]
    fn headings_drop_hashes_and_fade_by_depth() {
        let lines = render("# Top\n### Sub\n#nohashtag", 80, Style::new());
        let text = flat_text(&lines);
        assert_eq!(text[0], "Top");
        assert!(
            style_of(&lines, "Top")
                .add_modifier
                .contains(Modifier::UNDERLINED)
        );
        assert!(
            style_of(&lines, "Sub")
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert!(
            !style_of(&lines, "Sub")
                .add_modifier
                .contains(Modifier::UNDERLINED)
        );
        assert!(text.concat().contains("#nohashtag"), "no space = literal");
    }

    #[test]
    fn lists_hang_their_wrapped_lines() {
        let lines = render("- a rather long list item that wraps", 20, Style::new());
        let text = flat_text(&lines);
        assert!(text[0].starts_with("• a rather"), "{text:?}");
        assert!(text[1].starts_with("  "), "continuation hangs: {text:?}");
        // Nested + ordered markers survive.
        let text = flat_text(&render("  - nested\n2. second", 80, Style::new()));
        assert_eq!(text, ["  • nested", "2. second"]);
    }

    #[test]
    fn quotes_and_rules_render_as_chrome() {
        let lines = render("> wise words\n---", 10, Style::new());
        let text = flat_text(&lines);
        assert!(text[0].starts_with("▌ wise"), "{text:?}");
        assert!(
            style_of(&lines, "wise")
                .add_modifier
                .contains(Modifier::DIM)
        );
        assert_eq!(text.last().unwrap(), &"─".repeat(10));
    }

    #[test]
    fn code_panels_cover_the_full_width() {
        let lines = render("```rust\nfn main() {}\nlet x = 1;\n```", 24, Style::new());
        let text = flat_text(&lines);
        assert_eq!(
            text,
            [" fn main() {}           ", " let x = 1;             "]
        );
        for line in &lines {
            assert_eq!(line.spans[0].style.bg, Some(Color::Indexed(235)));
            assert_eq!(display_width(&flat_text(std::slice::from_ref(line))[0]), 24);
        }
    }

    #[test]
    fn an_unclosed_fence_streams_as_code() {
        // The closing ``` has not arrived yet: everything after the fence
        // renders as code until it does.
        let lines = render("text\n```\nstill code", 20, Style::new());
        assert_eq!(style_of(&lines, "still code").bg, Some(Color::Indexed(235)));
    }

    #[test]
    fn tables_align_and_style_the_header() {
        let md = "| Name | Wert |\n|---|---|\n| a | **1** |\n| lang_und_breit | 2 |";
        let lines = render(md, 80, Style::new());
        let text = flat_text(&lines);
        assert!(
            text[0].contains("Name") && text[0].contains("│"),
            "{text:?}"
        );
        assert!(text[1].contains("┼"), "header underline: {text:?}");
        assert!(
            style_of(&lines, "Name")
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert!(
            style_of(&lines, "1").add_modifier.contains(Modifier::BOLD),
            "inline markup inside cells"
        );
        // Every row is equally wide - alignment is the whole point.
        let w0 = display_width(&text[0]);
        assert!(
            text.iter().take(4).all(|t| display_width(t) == w0),
            "{text:?}"
        );
    }

    #[test]
    fn wide_tables_shrink_and_clip_with_ellipsis() {
        let md = "| col | eine sehr sehr sehr lange zelle |\n|---|---|\n| a | b |";
        let lines = render(md, 24, Style::new());
        for line in &lines {
            assert!(display_width(&flat_text(std::slice::from_ref(line))[0]) <= 24);
        }
        assert!(
            flat_text(&lines).concat().contains('…'),
            "clipped cell is marked"
        );
    }

    #[test]
    fn a_lone_pipe_line_is_not_a_table() {
        // Without a separator row it is just a paragraph containing pipes.
        let text = flat_text(&render("| not | a table", 80, Style::new())).concat();
        assert!(text.contains("| not | a table"), "{text}");
    }
}
