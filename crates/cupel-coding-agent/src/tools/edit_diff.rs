//! Text matching and diff engine for the `edit` tool. Port of pi's
//! `edit-diff.ts`.
//!
//! Why so much machinery for "replace old text with new text"? Because
//! models are imperfect copyists. They echo file content with smart quotes
//! flattened, trailing whitespace dropped, or Unicode dashes swapped - and a
//! byte-exact `indexOf` would reject the edit even though a human would call
//! it unambiguous. The pipeline:
//!
//! 1. Normalize line endings to LF (and strip a UTF-8 BOM) before matching;
//!    the original ending style is restored on write.
//! 2. Try an exact match first. If that fails, retry in *fuzzy-normalized*
//!    space (NFKC, trailing whitespace stripped, smart quotes/dashes/spaces
//!    folded to ASCII).
//! 3. Every `old_text` must be unique and edits must not overlap - both are
//!    hard errors with actionable messages, because silently picking one of
//!    several matches corrupts files.
//! 4. When fuzzy matching was used, only the LINES actually touched by a
//!    replacement are rewritten from normalized text; untouched lines keep
//!    their original bytes (`apply_replacements_preserving_unchanged_lines`).

use unicode_normalization::UnicodeNormalization as _;

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

/// Split off a UTF-8 BOM. The model never sees the invisible BOM, so it can
/// never include it in `old_text`; matching must happen without it.
#[must_use]
pub fn strip_bom(content: &str) -> (&'static str, &str) {
    content
        .strip_prefix('\u{FEFF}')
        .map_or(("", content), |rest| ("\u{FEFF}", rest))
}

// ---------------------------------------------------------------------------
// Fuzzy normalization
// ---------------------------------------------------------------------------

/// Normalize text for fuzzy matching: NFKC, strip trailing whitespace per
/// line, fold typographic quotes/dashes/spaces to their ASCII equivalents.
#[must_use]
pub fn normalize_for_fuzzy_match(text: &str) -> String {
    let nfkc: String = text.nfkc().collect();
    nfkc.split('\n')
        .map(|line| {
            line.trim_end()
                .chars()
                .map(|c| match c {
                    // Smart single quotes.
                    '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
                    // Smart double quotes.
                    '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
                    // Hyphens, dashes, minus signs.
                    '\u{2010}'..='\u{2015}' | '\u{2212}' => '-',
                    // Non-breaking and typographic spaces.
                    '\u{00A0}' | '\u{2002}'..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' => {
                        ' '
                    }
                    other => other,
                })
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------------------
// Matching + applying edits
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Edit {
    pub old_text: String,
    pub new_text: String,
}

/// One resolved replacement: byte offsets into the match base.
#[derive(Debug, Clone)]
struct Replacement {
    edit_index: usize,
    match_index: usize,
    match_length: usize,
    new_text: String,
}

#[derive(Debug)]
pub struct AppliedEdits {
    /// The LF-normalized content edits were matched against.
    pub base_content: String,
    /// The result of applying all edits.
    pub new_content: String,
}

fn count_occurrences(haystack: &str, needle: &str) -> usize {
    if needle.is_empty() {
        return 0;
    }
    haystack.matches(needle).count()
}

/// Apply replacements back-to-front so earlier offsets stay valid.
fn apply_replacements(content: &str, replacements: &[Replacement], offset: usize) -> String {
    let mut result = content.to_string();
    for replacement in replacements.iter().rev() {
        let start = replacement.match_index - offset;
        let end = start + replacement.match_length;
        result.replace_range(start..end, &replacement.new_text);
    }
    result
}

/// Byte spans of each line (line text + its terminator) in `content`.
fn line_spans(content: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut start = 0;
    for (i, byte) in content.bytes().enumerate() {
        if byte == b'\n' {
            spans.push((start, i + 1));
            start = i + 1;
        }
    }
    if start < content.len() {
        spans.push((start, content.len()));
    }
    spans
}

/// Overlay replacements computed against *normalized* `base_content` onto
/// `original_content`: only the line blocks a replacement touches are taken
/// from normalized space; every other line keeps its original bytes (so
/// e.g. trailing whitespace on untouched lines survives a fuzzy edit).
fn apply_replacements_preserving_unchanged_lines(
    original_content: &str,
    base_content: &str,
    replacements: &[Replacement],
) -> Result<String, String> {
    let original_spans = line_spans(original_content);
    let base_spans = line_spans(base_content);
    if original_spans.len() != base_spans.len() {
        return Err(
            "Cannot preserve unchanged lines because the base content has a different line count."
                .to_string(),
        );
    }

    // Group replacements into runs of touched lines.
    struct Group {
        start_line: usize,
        end_line: usize, // exclusive
        replacements: Vec<Replacement>,
    }
    let mut sorted: Vec<Replacement> = replacements.to_vec();
    sorted.sort_by_key(|r| r.match_index);

    let mut groups: Vec<Group> = Vec::new();
    for replacement in sorted {
        let start_offset = replacement.match_index;
        let end_offset = replacement.match_index + replacement.match_length;
        let start_line = base_spans
            .iter()
            .position(|(s, e)| start_offset >= *s && start_offset < *e)
            .ok_or("Replacement range is outside the base content.")?;
        let mut end_line = start_line;
        while end_line < base_spans.len() && base_spans[end_line].1 < end_offset {
            end_line += 1;
        }
        if end_line >= base_spans.len() {
            return Err("Replacement range is outside the base content.".to_string());
        }
        let end_line = end_line + 1;

        match groups.last_mut() {
            Some(group) if start_line < group.end_line => {
                group.end_line = group.end_line.max(end_line);
                group.replacements.push(replacement);
            }
            _ => groups.push(Group {
                start_line,
                end_line,
                replacements: vec![replacement],
            }),
        }
    }

    let mut result = String::new();
    let mut original_line = 0;
    for group in &groups {
        // Untouched prefix: original bytes.
        for span in &original_spans[original_line..group.start_line] {
            result.push_str(&original_content[span.0..span.1]);
        }
        // Touched block: normalized bytes with the replacements applied.
        let block_start = base_spans[group.start_line].0;
        let block_end = base_spans[group.end_line - 1].1;
        result.push_str(&apply_replacements(
            &base_content[block_start..block_end],
            &group.replacements,
            block_start,
        ));
        original_line = group.end_line;
    }
    for span in &original_spans[original_line..] {
        result.push_str(&original_content[span.0..span.1]);
    }
    Ok(result)
}

/// Match and apply all edits against LF-normalized content. Every edit is
/// matched against the ORIGINAL content (not incrementally), then applied
/// back-to-front. See module docs for the fuzzy path.
pub fn apply_edits(
    normalized_content: &str,
    edits: &[Edit],
    path: &str,
) -> Result<AppliedEdits, String> {
    let total = edits.len();
    let edits: Vec<Edit> = edits
        .iter()
        .map(|e| Edit {
            old_text: normalize_to_lf(&e.old_text),
            new_text: normalize_to_lf(&e.new_text),
        })
        .collect();

    for (i, edit) in edits.iter().enumerate() {
        if edit.old_text.is_empty() {
            return Err(if total == 1 {
                format!("oldText must not be empty in {path}.")
            } else {
                format!("edits[{i}].oldText must not be empty in {path}.")
            });
        }
    }

    // Decide the match space: if ANY edit needs fuzzy matching, all edits
    // are matched in fuzzy space so their offsets are consistent.
    let needs_fuzzy = edits
        .iter()
        .any(|edit| !normalized_content.contains(edit.old_text.as_str()));
    let base = if needs_fuzzy {
        normalize_for_fuzzy_match(normalized_content)
    } else {
        normalized_content.to_string()
    };

    let mut matched: Vec<Replacement> = Vec::new();
    for (i, edit) in edits.iter().enumerate() {
        let needle = if needs_fuzzy {
            normalize_for_fuzzy_match(&edit.old_text)
        } else {
            edit.old_text.clone()
        };
        let Some(index) = base.find(&needle) else {
            return Err(if total == 1 {
                format!(
                    "Could not find the exact text in {path}. The old text must match exactly \
                     including all whitespace and newlines."
                )
            } else {
                format!(
                    "Could not find edits[{i}] in {path}. The oldText must match exactly \
                     including all whitespace and newlines."
                )
            });
        };
        let occurrences = count_occurrences(&base, &needle);
        if occurrences > 1 {
            return Err(if total == 1 {
                format!(
                    "Found {occurrences} occurrences of the text in {path}. The text must be \
                     unique. Please provide more context to make it unique."
                )
            } else {
                format!(
                    "Found {occurrences} occurrences of edits[{i}] in {path}. Each oldText must \
                     be unique. Please provide more context to make it unique."
                )
            });
        }
        matched.push(Replacement {
            edit_index: i,
            match_index: index,
            match_length: needle.len(),
            new_text: edit.new_text.clone(),
        });
    }

    // Overlap check: silently applying overlapping edits corrupts files.
    matched.sort_by_key(|r| r.match_index);
    for pair in matched.windows(2) {
        // Slice-pattern destructuring instead of pair[0]/pair[1]: no bounds
        // checks, and the compiler proves the window size for us.
        let [previous, current] = pair else { continue };
        if previous.match_index + previous.match_length > current.match_index {
            return Err(format!(
                "edits[{}] and edits[{}] overlap in {path}. Merge them into one edit or target \
                 disjoint regions.",
                previous.edit_index, current.edit_index
            ));
        }
    }

    let new_content = if needs_fuzzy {
        apply_replacements_preserving_unchanged_lines(normalized_content, &base, &matched)?
    } else {
        apply_replacements(&base, &matched, 0)
    };

    if new_content == normalized_content {
        return Err(if total == 1 {
            format!(
                "No changes made to {path}. The replacement produced identical content. This \
                 might indicate an issue with special characters or the text not existing as \
                 expected."
            )
        } else {
            format!("No changes made to {path}. The replacements produced identical content.")
        });
    }

    Ok(AppliedEdits {
        base_content: normalized_content.to_string(),
        new_content,
    })
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
    // around each cluster - exactly the shape pi builds by hand.
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

/// A standard unified patch (the `diff -u` format).
#[must_use]
pub fn generate_unified_patch(path: &str, old: &str, new: &str, context_lines: usize) -> String {
    similar::TextDiff::from_lines(old, new)
        .unified_diff()
        .context_radius(context_lines)
        .header(path, path)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(old_text: &str, new_text: &str) -> Edit {
        Edit {
            old_text: old_text.to_string(),
            new_text: new_text.to_string(),
        }
    }

    #[test]
    fn exact_single_edit() {
        let result = apply_edits(
            "fn main() {\n    old();\n}\n",
            &[edit("old()", "new()")],
            "f.rs",
        )
        .expect("edit applies");
        assert_eq!(result.new_content, "fn main() {\n    new();\n}\n");
    }

    #[test]
    fn multiple_disjoint_edits_match_against_original() {
        let content = "alpha\nbeta\ngamma\n";
        let result = apply_edits(
            content,
            &[edit("gamma", "GAMMA"), edit("alpha", "ALPHA")],
            "f.txt",
        )
        .expect("edits apply");
        assert_eq!(result.new_content, "ALPHA\nbeta\nGAMMA\n");
    }

    #[test]
    fn ambiguous_match_is_an_error() {
        let err = apply_edits("x\nx\n", &[edit("x", "y")], "f.txt").unwrap_err();
        assert!(err.contains("2 occurrences"), "got: {err}");
    }

    #[test]
    fn missing_text_is_an_error() {
        let err = apply_edits("hello\n", &[edit("goodbye", "x")], "f.txt").unwrap_err();
        assert!(err.contains("Could not find"), "got: {err}");
    }

    #[test]
    fn overlapping_edits_are_an_error() {
        let err =
            apply_edits("abcdef\n", &[edit("abcd", "x"), edit("cdef", "y")], "f.txt").unwrap_err();
        assert!(err.contains("overlap"), "got: {err}");
    }

    #[test]
    fn no_op_edit_is_an_error() {
        let err = apply_edits("same\n", &[edit("same", "same")], "f.txt").unwrap_err();
        assert!(err.contains("No changes"), "got: {err}");
    }

    #[test]
    fn fuzzy_matches_smart_quotes() {
        // File has a typographic apostrophe; the model echoes an ASCII one.
        let content = "let s = \u{2018}hello\u{2019};\nuntouched\t\n";
        let result = apply_edits(
            content,
            &[edit("let s = 'hello';", "let s = 'bye';")],
            "f.rs",
        )
        .expect("fuzzy edit applies");
        assert!(result.new_content.contains("'bye'"));
        // The untouched line keeps its original trailing tab even though
        // fuzzy normalization would have stripped it.
        assert!(result.new_content.contains("untouched\t\n"));
    }

    #[test]
    fn fuzzy_matches_trailing_whitespace_difference() {
        let content = "line one   \nline two\n"; // trailing spaces in file
        let result = apply_edits(content, &[edit("line one", "line 1")], "f.txt")
            .expect("fuzzy edit applies");
        assert!(result.new_content.starts_with("line 1"));
    }

    #[test]
    fn bom_and_crlf_round_trip() {
        let raw = "\u{FEFF}a\r\nb\r\n";
        let (bom, text) = strip_bom(raw);
        assert_eq!(bom, "\u{FEFF}");
        let ending = detect_line_ending(text);
        assert_eq!(ending, LineEnding::CrLf);
        let normalized = normalize_to_lf(text);
        let result = apply_edits(&normalized, &[edit("a", "x")], "f.txt").expect("applies");
        let restored = format!("{bom}{}", restore_line_endings(&result.new_content, ending));
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

    #[test]
    fn unified_patch_has_hunk_header() {
        let patch = generate_unified_patch("f.txt", "a\nb\n", "a\nc\n", 4);
        assert!(patch.contains("@@"), "got:\n{patch}");
        assert!(patch.contains("-b"), "got:\n{patch}");
        assert!(patch.contains("+c"), "got:\n{patch}");
    }
}
