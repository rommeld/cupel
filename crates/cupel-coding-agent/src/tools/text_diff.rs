//! Text normalization and diff rendering behind [`super::apply_patch`].
//!
//! Patch hunks are matched against LF-normalized, BOM-free text; the
//! original line-ending style and BOM are restored on write, so a CRLF file
//! stays CRLF and a BOM survives a patch it is invisible to the model, whose
//! context lines never carry it. [`generate_diff_string`] renders the result
//! for the transcript: a display diff with line numbers and limited context.

// ---------------------------------------------------------------------------
// Line-ending and BOM handling
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineEnding {
    Lf,
    CrLf,
}

/// Detect the file's dominant line ending from its first line break.
#[must_use]
pub fn detect_line_ending(content: &str) -> LineEnding {
    match (content.find("\r\n"), content.find('\n')) {
        (Some(crlf), Some(lf)) if crlf < lf => LineEnding::CrLf,
        _ => LineEnding::Lf,
    }
}

#[must_use]
pub fn normalize_to_lf(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

#[must_use]
pub fn restore_line_endings(text: &str, ending: LineEnding) -> String {
    match ending {
        LineEnding::Lf => text.to_string(),
        LineEnding::CrLf => text.replace('\n', "\r\n"),
    }
}

/// Split off a UTF-8 BOM. The model never sees the invisible BOM, so its
/// patch context can never include it; matching must happen without it.
#[must_use]
pub fn strip_bom(content: &str) -> (&'static str, &str) {
    content
        .strip_prefix('\u{FEFF}')
        .map_or(("", content), |rest| ("\u{FEFF}", rest))
}

// ---------------------------------------------------------------------------
// Diff rendering
// ---------------------------------------------------------------------------

pub struct DiffString {
    pub diff: String,
    /// Line number of the first change in the NEW file (editor navigation).
    pub first_changed_line: Option<usize>,
}

/// A display diff with line numbers and limited context, e.g.
/// ```text
///  10 unchanged
/// -11 removed line
/// +11 added line
/// ```
/// Built on the `similar` crate's line diff (pi uses the `diff` npm package).
#[must_use]
pub fn generate_diff_string(old: &str, new: &str, context_lines: usize) -> DiffString {
    let diff = similar::TextDiff::from_lines(old, new);
    let width = old
        .lines()
        .count()
        .max(new.lines().count())
        .to_string()
        .len();

    let mut output: Vec<String> = Vec::new();
    let mut first_changed_line: Option<usize> = None;
    // The 1-based NEW-file line where the next line would land. Needed for
    // deletions: a deleted line has no new_index (it doesn't exist in the
    // new file), but "where the change appears in the new file" is exactly
    // this running position.
    let mut next_new_line = 1_usize;

    // `grouped_ops` clusters changes and gives `context_lines` of equal lines
    // around each cluster — exactly the shape pi builds by hand.
    for group in diff.grouped_ops(context_lines) {
        for (op_index, op) in group.iter().enumerate() {
            for change in diff.iter_changes(op) {
                match change.tag() {
                    similar::ChangeTag::Delete => {
                        let line_number = change.old_index().unwrap_or(0) + 1;
                        output.push(format!(
                            "-{line_number:>width$} {}",
                            change.value().trim_end_matches('\n')
                        ));
                        if first_changed_line.is_none() {
                            first_changed_line = Some(next_new_line);
                        }
                    }
                    similar::ChangeTag::Insert => {
                        let line_number = change.new_index().unwrap_or(0) + 1;
                        output.push(format!(
                            "+{line_number:>width$} {}",
                            change.value().trim_end_matches('\n')
                        ));
                        if first_changed_line.is_none() {
                            first_changed_line = Some(line_number);
                        }
                        next_new_line = line_number + 1;
                    }
                    similar::ChangeTag::Equal => {
                        let line_number = change.old_index().unwrap_or(0) + 1;
                        output.push(format!(
                            " {line_number:>width$} {}",
                            change.value().trim_end_matches('\n')
                        ));
                        next_new_line = change.new_index().unwrap_or(0) + 2;
                    }
                }
            }
            // Gap marker between change clusters within one group.
            if op_index + 1 < group.len() && matches!(op, similar::DiffOp::Equal { .. }) {
                // similar already limits equal runs to the context window,
                // so no explicit "..." is needed inside a group.
            }
        }
        output.push(format!(" {} ...", " ".repeat(width)));
    }
    // Drop the trailing group separator.
    if output.last().is_some_and(|l| l.ends_with("...")) {
        output.pop();
    }

    DiffString {
        diff: output.join("\n"),
        first_changed_line,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bom_and_crlf_round_trip() {
        let raw = "\u{FEFF}a\r\nb\r\n";
        let (bom, text) = strip_bom(raw);
        assert_eq!(bom, "\u{FEFF}");
        assert_eq!(detect_line_ending(text), LineEnding::CrLf);
        let normalized = normalize_to_lf(text);
        assert_eq!(normalized, "a\nb\n");
        let edited = normalized.replace('a', "x");
        let restored = format!("{bom}{}", restore_line_endings(&edited, LineEnding::CrLf));
        assert_eq!(restored, "\u{FEFF}x\r\nb\r\n");
    }

    #[test]
    fn diff_string_shows_line_numbers_and_context() {
        let old = "a\nb\nc\nd\ne\n";
        let new = "a\nb\nC\nd\ne\n";
        let diff = generate_diff_string(old, new, 1);
        assert!(diff.diff.contains("-3 c"), "got:\n{}", diff.diff);
        assert!(diff.diff.contains("+3 C"), "got:\n{}", diff.diff);
        assert_eq!(diff.first_changed_line, Some(3));
    }
}
