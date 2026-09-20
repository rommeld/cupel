//! Apply the `@@` chunks of an `*** Update File:` hunk to a file's text.
//!
//! A port of Codex's seek_sequence.rs and file_update.rs. The tool hands this module
//! BOM-free, LF-normalized text (the round trip lives in [`super::text_diff`]); line
//! endings and the BOM are restored by the tool on write.
//!
//! Two ideas carry the whole module:
//!
//! 1. A chunk is located, not addressed. There are no line numbers in the format;
//! `old_lines` (context + removed lines) are searched for in the file, starting after
//! the previous chunk, with decreasing strictness. Ignoring trailing whitespce,
//! ignoring surrounding whitespace, then with typographic punctuation folded to ASCII.
//! Models drop trailing spaces and flatten smart quotes.
//! 2. Matchin and rewriting are separate passes. Every chunk is resolved against the
//! original lines into a `(start, old_len, new_lines)` replacement. The replacements
//! are then applied back to front so earlier indices stay valid. A chunk that does
//! not match absorts before anything is rewritten.

use crate::tools::patch_parser::UpdateFileChunk;
use unicode_normalization::UnicodeNormalization as _;

/// `(start_index, old_len, new_lines)`: replace `old_len` lines from `start_index`
/// with `new_lines`. `old_len == 0` is a pure inseration.
type Replacement = (usize, usize, Vec<String>);

/// Find `pattern` as a run of consecutive lines in `lines`, at or after `start`.
/// Returns the index of the first matching line.
///
/// With `eof` the search begins where a match would end the file, so a chunk marked
/// `*** End of File` lands on the last occurence even when an identical block appears
/// earlier (Codex tries only that position in this mode; a miss falls through to the
/// same start, i.e. to `None`).
///
/// Edge cases, defensively: an empty pattern matches at `start`; a pattern longer than
/// the fle cannot match.
#[must_use]
pub fn seek_sequence(
    lines: &[String],
    pattern: &[String],
    start: usize,
    eof: bool,
) -> Option<usize> {
    if pattern.is_empty() {
        return Some(start);
    }
    if pattern.len() > lines.len() {
        return None;
    }
    let last_start = lines.len() - pattern.len();
    let search_start = if eof { last_start } else { start };

    // Four passes of decreasing strictness. A pass is a line comparison. The loop
    // below is the same for all of them, so they are data.
    let passes: [fn(&str, &str) -> bool; 4] = [
        |file_line, wanted| file_line == wanted,
        |file_line, wanted| file_line.trim_end() == wanted.trim_end(),
        |file_line, wanted| file_line.trim() == wanted.trim(),
        |file_line, wanted| normalise(file_line) == normalise(wanted),
    ];
    for equal in passes {
        for index in search_start..=last_start {
            let matches = pattern
                .iter()
                .enumerate()
                .all(|(offset, wanted)| equal(&lines[index + offset], wanted));
            if matches {
                return Some(index);
            }
        }
    }
    None
}

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

/// The most permissive comparison key: surrounding whitespace dropped and smart
/// quotes, dashes and typographic spaces folded as ASCII, so a patch typed in
/// plain ASCII still applies to a file with typographic punctuation. The folding
/// table came over from the former edit tool; NFKC on top of Codex's table is the
/// one difference.
fn normalise(line: &str) -> String {
    normalize_for_fuzzy_match(line.trim())
}

/// Rewrite `original` (LF-normalize) with `chunks`, `path` only labels error messages.
///
/// Codex semantics, kept deliberately:
/// - The result always ends with a newline.
/// - A chunk with no context and no removed lines is appended at the end of the file,
/// whatever `@@` said.
/// - `old_lines` may end with an empty line that stands for the file's final newline.
pub fn apply_chunks(
    original: &str,
    chunks: &[UpdateFileChunk],
    path: &str,
) -> Result<String, String> {
    let mut lines: Vec<String> = original.split('\n').map(String::from).collect();
    if lines.last().is_some_and(String::is_empty) {
        lines.pop();
    }
    let replacements = compute_replacements(&lines, chunks, path)?;
    let mut lines = apply_replacements(lines, &replacements);
    if !lines.last().is_some_and(String::is_empty) {
        lines.push(String::new());
    }
    Ok(lines.join("\n"))
}

/// Resolve every chunk against the original lines, in order. `line_index` only ever
/// moves forward.
fn compute_replacements(
    lines: &[String],
    chunks: &[UpdateFileChunk],
    path: &str,
) -> Result<Vec<Replacement>, String> {
    let mut replacements: Vec<Replacement> = Vec::new();
    let mut line_index = 0;

    for chunk in chunks {
        // `@@` fn name()`: jump to the line that matches, then search below it.
        if let Some(context) = &chunk.change_context {
            let Some(index) =
                seek_sequence(lines, std::slice::from_ref(context), line_index, false)
            else {
                return Err(format!("Failed to find context '{context}' in {path}"));
            };
            line_index = index + 1;
        }

        if chunk.old_lines.is_empty() {
            // Nothing to anchor on: append. (Before a trailing empty line if the file
            // has one, so the blank line stays last.)
            let insertion = if lines.last().is_some_and(String::is_empty) {
                lines.len() - 1
            } else {
                lines.len()
            };
            replacements.push((insertion, 0, chunk.new_lines.clone()));
            continue;
        }

        let mut pattern: &[String] = &chunk.old_lines;
        let mut new_lines: &[String] = &chunk.new_lines;
        let mut found = seek_sequence(lines, pattern, line_index, chunk.is_end_of_file);
        if found.is_none() && pattern.last().is_some_and(String::is_empty) {
            // The trailing empty line stood for the final newline, which is not a line
            // of its own after `split`; retry without it.
            pattern = &pattern[..pattern.len() - 1];
            if new_lines.last().is_some_and(String::is_empty) {
                new_lines = &new_lines[..new_lines.len() - 1];
            }
            found = seek_sequence(lines, pattern, line_index, chunk.is_end_of_file);
        }
        let Some(start) = found else {
            return Err(format!(
                "Failed to find expected lines in {path}:\n{}",
                chunk.old_lines.join("\n")
            ));
        };
        replacements.push((start, pattern.len(), new_lines.to_vec()));
        line_index = start + pattern.len();
    }

    replacements.sort_by_key(|(start, _, _)| *start);
    Ok(replacements)
}

/// Apply back to front: a replacement never shifts the indices of the ones before it.
/// `splice` removes the old range and inserts the new lines in one move of the tail.
fn apply_replacements(mut lines: Vec<String>, replacements: &[Replacement]) -> Vec<String> {
    for (start, old_len, new_lines) in replacements.iter().rev() {
        let end = (start + old_len).min(lines.len());
        drop(lines.splice(*start..end, new_lines.iter().cloned()));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lines(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    fn chunk(context: Option<&str>, old: &[&str], new: &[&str], eof: bool) -> UpdateFileChunk {
        UpdateFileChunk {
            change_context: context.map(str::to_string),
            old_lines: lines(old),
            new_lines: lines(new),
            is_end_of_file: eof,
        }
    }

    #[test]
    fn seek_exact_match() {
        assert_eq!(
            seek_sequence(
                &lines(&["foo", "bar", "baz"]),
                &lines(&["bar", "baz"]),
                0,
                false
            ),
            Some(1)
        );
    }

    #[test]
    fn seek_ignores_trailing_then_surrounding_whitespace() {
        assert_eq!(
            seek_sequence(
                &lines(&["foo   ", "bar\t\t"]),
                &lines(&["foo", "bar"]),
                0,
                false
            ),
            Some(0)
        );
        assert_eq!(
            seek_sequence(
                &lines(&["    foo   ", "   bar\t"]),
                &lines(&["foo", "bar"]),
                0,
                false
            ),
            Some(0)
        );
    }

    #[test]
    fn seek_folds_typographic_punctuation() {
        let file = lines(&["import asyncio  # local import \u{2013} avoids top\u{2011}level dep"]);
        let wanted = lines(&["import asyncio  # local import - avoids top-level dep"]);
        assert_eq!(seek_sequence(&file, &wanted, 0, false), Some(0));
    }

    #[test]
    fn seek_pattern_longer_than_input_is_none_not_a_panic() {
        assert_eq!(
            seek_sequence(
                &lines(&["just one line"]),
                &lines(&["too", "many", "lines"]),
                0,
                false
            ),
            None
        );
        assert_eq!(seek_sequence(&lines(&["a"]), &[], 7, false), Some(7));
    }

    #[test]
    fn seek_from_start_skips_earlier_occurrences_and_eof_takes_the_last() {
        let file = lines(&["x", "y", "x", "y"]);
        assert_eq!(seek_sequence(&file, &lines(&["x"]), 1, false), Some(2));
        assert_eq!(seek_sequence(&file, &lines(&["y"]), 0, true), Some(3));
        // eof pins the search to the end: an earlier-only block is a miss.
        assert_eq!(
            seek_sequence(&file, &lines(&["x", "y", "x"]), 0, true),
            None
        );
    }

    #[test]
    fn multiple_chunks_apply_in_file_order() {
        // Codex fixture 003.
        let result = apply_chunks(
            "line1\nline2\nline3\nline4\n",
            &[
                chunk(None, &["line2"], &["changed2"], false),
                chunk(None, &["line4"], &["changed4"], false),
            ],
            "multi.txt",
        )
        .expect("applies");
        assert_eq!(result, "line1\nchanged2\nline3\nchanged4\n");
    }

    #[test]
    fn context_and_change_context_locate_the_chunk() {
        let result = apply_chunks(
            "a\nb\nc\nd\ne\nf\n",
            &[
                chunk(None, &["a", "b"], &["a", "B"], false),
                chunk(None, &["c", "d", "e"], &["c", "d", "E"], false),
                chunk(None, &["f"], &["f", "g"], true),
            ],
            "interleaved.txt",
        )
        .expect("applies");
        assert_eq!(result, "a\nB\nc\nd\nE\nf\ng\n");

        let result = apply_chunks(
            "fn a() {\n    1\n}\nfn b() {\n    1\n}\n",
            &[chunk(Some("fn b() {"), &["    1"], &["    2"], false)],
            "f.rs",
        )
        .expect("applies");
        assert_eq!(result, "fn a() {\n    1\n}\nfn b() {\n    2\n}\n");
    }

    #[test]
    fn missing_lines_and_missing_context_are_errors() {
        // Codex fixture 006.
        let err = apply_chunks(
            "line1\nline2\n",
            &[chunk(None, &["missing"], &["changed"], false)],
            "modify.txt",
        )
        .unwrap_err();
        assert_eq!(err, "Failed to find expected lines in modify.txt:\nmissing");
        let err = apply_chunks(
            "line1\n",
            &[chunk(Some("nowhere"), &["line1"], &["x"], false)],
            "f.txt",
        )
        .unwrap_err();
        assert_eq!(err, "Failed to find context 'nowhere' in f.txt");
    }

    #[test]
    fn result_always_ends_with_a_newline() {
        // Codex fixture 014.
        let result = apply_chunks(
            "no newline at end",
            &[chunk(
                None,
                &["no newline at end"],
                &["first line", "second line"],
                false,
            )],
            "no_newline.txt",
        )
        .expect("applies");
        assert_eq!(result, "first line\nsecond line\n");
    }

    #[test]
    fn pure_additions_append_at_the_end() {
        // Codex fixture 016 ...
        let result = apply_chunks(
            "line1\nline2\n",
            &[chunk(None, &[], &["added line 1", "added line 2"], false)],
            "input.txt",
        )
        .expect("applies");
        assert_eq!(result, "line1\nline2\nadded line 1\nadded line 2\n");
        // ... even when a removal follows in a later chunk.
        let result = apply_chunks(
            "line1\nline2\nline3\n",
            &[
                chunk(None, &[], &["after-context", "second-line"], false),
                chunk(
                    None,
                    &["line1", "line2", "line3"],
                    &["line1", "line2-replacement"],
                    false,
                ),
            ],
            "panic.txt",
        )
        .expect("applies");
        assert_eq!(
            result,
            "line1\nline2-replacement\nafter-context\nsecond-line\n"
        );
    }

    #[test]
    fn deletion_only_and_end_of_file_chunks() {
        // Codex fixtures 021 and 022.
        let result = apply_chunks(
            "line1\nline2\nline3\n",
            &[chunk(
                None,
                &["line1", "line2", "line3"],
                &["line1", "line3"],
                false,
            )],
            "lines.txt",
        )
        .expect("applies");
        assert_eq!(result, "line1\nline3\n");
        let result = apply_chunks(
            "first\nsecond\n",
            &[chunk(
                None,
                &["first", "second"],
                &["first", "second updated"],
                true,
            )],
            "tail.txt",
        )
        .expect("applies");
        assert_eq!(result, "first\nsecond updated\n");
    }

    #[test]
    fn trailing_empty_old_line_is_retried_without_it() {
        let result = apply_chunks(
            "a\nb\n",
            &[chunk(None, &["b", ""], &["B", ""], false)],
            "f.txt",
        )
        .expect("applies");
        assert_eq!(result, "a\nB\n");
    }
}
