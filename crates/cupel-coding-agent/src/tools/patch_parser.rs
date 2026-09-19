//! The apply_patch enevlope: text -> hunks.
//!
//! This is a port of Codex's patch parser, minus streaming (TODO), environment ids
//! and the shell heredoc detection. The format, from Codex's Lark Grammer:
//!
//! ```text
//! start: begin_patch hunk+ end_patch
//! begin_patch: "*** Begin Patch" LF
//! end_patch: "*** End Patch" LF?
//!
//! hunk: add_hunk | delete_hunk | update_hunk
//! add_hunk: "*** Add File: " filename LF add_line+
//! delete_hunk: "*** Delete File: " filename LF
//! update_hunk: "*** Update File: " filename LF change_move? change?
//!
//! change_move: "*** Move to: " filename LF
//! change: (change_context | change_line)+ eof_line?
//! chagen_context: ("@@" | "@@ " /(.+)/) LF
//! chagen_line: ("+" | "-" | " ") /(.*)/ LF
//! eof_line: "*** End of File" LF
//! ```
//!
//! Like Codex, the parser is more lenient than the grammer: markers may carry leading/
//! trailing whitespace, a bare empty line inside an update hunk counts as an empty
//! context line, and a `<<EOF` heredoc wrapper around the whole patch stripped (some
//! models wrap the patch the way Codex's shell tool taught them).
//!
//! Parsing only checks the shape of the patch. Whether the files exist and the hunks
//! apply is decided later.

use thiserror::Error;

pub const BEGIN_PATCH_MARKER: &str = "*** Begin Patch";
pub const END_PATCH_MARKER: &str = "*** End Patch";
pub const ADD_FILE_MARKER: &str = "*** Add File: ";
pub const DELETE_FILE_MARKER: &str = "*** Delete File: ";
pub const UPDATE_FILE_MARKER: &str = "*** Update File: ";
pub const MOVE_TO_MARKER: &str = "*** Move to: ";
pub const EOF_MARKER: &str = "*** End of File";
pub const CHANGE_CONTEXT_MARKER: &str = "@@ ";
pub const EMPTY_CHANGE_CONTEXT_MARKER: &str = "@@";

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ParseError {
    #[error("invalid patch: {0}")]
    InvalidPatch(String),
    #[error("invalid hunk at line {line_number}, {message}")]
    InvalidHunk { message: String, line_number: usize },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hunk {
    AddFile {
        path: String,
        contents: String,
    },
    DeleteFile {
        path: String,
    },
    UpdateFile {
        path: String,
        move_path: Option<String>,
        chunks: Vec<UpdateFileChunk>,
    },
}

impl Hunk {
    #[must_use]
    pub fn path(&self) -> &str {
        match self {
            Self::AddFile { path, .. } | Self::DeleteFile { path } => path,
            Self::UpdateFile {
                path, move_path, ..
            } => move_path.as_deref().unwrap_or(path),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UpdateFileChunk {
    pub change_context: Option<String>,
    pub old_lines: Vec<String>,
    pub new_lines: Vec<String>,
    pub is_end_of_file: bool,
}

pub fn parse_patch(text: &str) -> Result<Vec<Hunk>, ParseError> {
    // `lines()` already drops a trainling `\r` before each `\n`, so a patch typed
    // with CRLF endings parses line an LF one.
    let lines: Vec<&str> = text.trim().lines().collect();
    let lines = strip_heredoc(&lines)?;
    let mut parser = Parser::default();
    let last = lines.len().saturating_sub(1);
    for (index, line) in lines.iter().enumerate() {
        parser.line_number += 1;
        if index == last && line.trim() == END_PATCH_MARKER {
            // Inside an update hunk an indented marker is a context line.
            parser.ensure_update_hunk_is_not_empty(line.trim())?;
            parser.mode = Mode::EndedPatch;
            continue;
        }
        parser.process_line(line)?;
    }
    if parser.mode != Mode::EndedPatch {
        return Err(ParseError::InvalidPatch(
            "The last line of the patch must be '*** End Patch'".to_string(),
        ));
    }
    Ok(parser.hunks)
}

fn strip_heredoc<'a>(lines: &'a [&'a str]) -> Result<&'a [&'a str], ParseError> {
    let strict_error = match check_boundaries(lines) {
        Ok(()) => return Ok(lines),
        Err(error) => error,
    };
    if let [first, .., last] = lines
        && matches!(*first, "<<EOF" | "<<'EOF'" | "<<\"EOF\"")
        && last.ends_with("EOF")
        && lines.len() >= 4
    {
        let inner = &lines[1..lines.len() - 1];
        check_boundaries(inner)?;
        return Ok(inner);
    }
    Err(strict_error)
}

/// First line `*** Begin Patch', last line `***End Patch`, whitespace around either
/// tolerated.
fn check_boundaries(lines: &[&str]) -> Result<(), ParseError> {
    let (first, last) = match lines {
        [] => (None, None),
        [only] => (Some(only.trim()), Some(only.trim())),
        [first, .., last] => (Some(first.trim()), Some(last.trim())),
    };
    match (first, last) {
        (Some(BEGIN_PATCH_MARKER), Some(END_PATCH_MARKER)) => Ok(()),
        (Some(first), _) if first != BEGIN_PATCH_MARKER => Err(ParseError::InvalidPatch(
            "The first line of the patch must be '*** Begin Patch'".to_string(),
        )),
        _ => Err(ParseError::InvalidPatch(
            "The last line of the patch must be '*** End Patch'".to_string(),
        )),
    }
}

/// Where the parser is inside the envelope. Each variant is one kind of line the
/// parser expects next, so an impossible transition is an error and not a slinet
/// misparse.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Mode {
    #[default]
    NotStarted,
    StartedPatch,
    AddFile,
    DeleteFile,
    UpdateFile {
        hunk_line_number: usize,
    },
    EndedPatch,
}

#[derive(Debug, Default)]
struct Parser {
    hunks: Vec<Hunk>,
    mode: Mode,
    line_number: usize,
}

const VALID_HEADERS: &str = "Valid hunk headers: '*** Add File: {path}', '*** Delete File: \
                            {path}', '*** Update File: {path}'";
const LINE_PREFIXES: &str = "Every line should start with ' ' (context line), '+' (added \
                            line), or '-' (removed line)";

impl Parser {
    fn invalid_hunk(&self, message: String) -> ParseError {
        ParseError::InvalidHunk {
            message,
            line_number: self.line_number,
        }
    }

    /// The update hunk that is being closed must carry at least one chunk lines in it.
    fn ensure_update_hunk_is_not_empty(&self, line: &str) -> Result<(), ParseError> {
        let Some(Hunk::UpdateFile { path, chunks, .. }) = self.hunks.last() else {
            return Ok(());
        };
        if chunks.is_empty()
            && let Mode::UpdateFile { hunk_line_number } = self.mode
        {
            return Err(ParseError::InvalidHunk {
                message: format!("Update file hunk for path '{path}' is empty"),
                line_number: hunk_line_number,
            });
        }
        if chunks
            .last()
            .is_some_and(|chunk| chunk.old_lines.is_empty() && chunk.new_lines.is_empty())
        {
            if line == END_PATCH_MARKER {
                return Err(self.invalid_hunk("Update hunk does not contain any lines".to_string()));
            }
            return Err(self.invalid_hunk(format!(
                "Unexpected line found in update hunk: '{line}'. {LINE_PREFIXES}"
            )));
        }
        Ok(())
    }

    /// Section headers and the end marker are valid in every mode after `*** Begin
    /// Patch`. Returns `Ok(true)` when `trimmed` was one of them.
    fn handle_headers(&mut self, trimmed: &str) -> Result<bool, ParseError> {
        if trimmed == END_PATCH_MARKER {
            self.ensure_update_hunk_is_not_empty(trimmed)?;
            self.mode = Mode::EndedPatch;
            return Ok(true);
        }
        if let Some(path) = trimmed.strip_prefix(ADD_FILE_MARKER) {
            self.ensure_update_hunk_is_not_empty(trimmed)?;
            self.hunks.push(Hunk::AddFile {
                path: path.to_string(),
                contents: String::new(),
            });
            self.mode = Mode::AddFile;
            return Ok(true);
        }
        if let Some(path) = trimmed.strip_prefix(DELETE_FILE_MARKER) {
            self.ensure_update_hunk_is_not_empty(trimmed)?;
            self.hunks.push(Hunk::DeleteFile {
                path: path.to_string(),
            });
            self.mode = Mode::DeleteFile;
            return Ok(true);
        }
        if let Some(path) = trimmed.strip_prefix(UPDATE_FILE_MARKER) {
            self.ensure_update_hunk_is_not_empty(trimmed)?;
            self.hunks.push(Hunk::UpdateFile {
                path: path.to_string(),
                move_path: None,
                chunks: Vec::new(),
            });
            self.mode = Mode::UpdateFile {
                hunk_line_number: self.line_number,
            };
            return Ok(true);
        }
        Ok(false)
    }

    fn process_line(&mut self, line: &str) -> Result<(), ParseError> {
        let trimmed = line.trim();
        match self.mode {
            Mode::NotStarted => {
                if trimmed == BEGIN_PATCH_MARKER {
                    self.mode = Mode::StartedPatch;
                    return Ok(());
                }
                Err(ParseError::InvalidPatch(
                    "The first line of the patch must be '*** Begin Patch'".to_string(),
                ))
            }
            Mode::StartedPatch | Mode::DeleteFile => {
                if self.handle_headers(trimmed)? {
                    return Ok(());
                }
                Err(self.invalid_hunk(format!(
                    "'{trimmed}' is not a valid hunk header. {VALID_HEADERS}"
                )))
            }
            Mode::AddFile => {
                if self.handle_headers(trimmed)? {
                    return Ok(());
                }
                if let Some(content) = line.strip_prefix('+')
                    && let Some(Hunk::AddFile { contents, .. }) = self.hunks.last_mut()
                {
                    contents.push_str(content);
                    contents.push('\n');
                    return Ok(());
                }
                Err(self.invalid_hunk(format!(
                    "'{trimmed}' is not a valid hunk header. {VALID_HEADERS}"
                )))
            }
            Mode::UpdateFile { hunk_line_number } => {
                self.process_update_line(line, hunk_line_number)
            }
            Mode::EndedPatch => {
                if trimmed.is_empty() {
                    return Ok(());
                }
                Err(ParseError::InvalidPatch(
                    "The last line of the patch must be '*** End Patch'".to_string(),
                ))
            }
        }
    }

    fn process_update_line(
        &mut self,
        line: &str,
        hunk_line_number: usize,
    ) -> Result<(), ParseError> {
        let update_line = line.trim_end();
        if self.handle_headers(update_line)? {
            return Ok(());
        }
        let Some(Hunk::UpdateFile {
            move_path, chunks, ..
        }) = self.hunks.last_mut()
        else {
            return Err(ParseError::InvalidHunk {
                message: "update hunk without header".to_string(),
                line_number: hunk_line_number,
            });
        };
        let last_chunk_is_empty = chunks
            .last()
            .is_some_and(|chunk| chunk.old_lines.is_empty() && chunk.new_lines.is_empty());
        let is_context_marker = update_line == EMPTY_CHANGE_CONTEXT_MARKER
            || update_line.starts_with(CHANGE_CONTEXT_MARKER);

        if chunks.last().is_some_and(|chunk| chunk.is_end_of_file) {
            if update_line.is_empty() {
                return Ok(());
            }
            if !is_context_marker {
                return Err(ParseError::InvalidHunk {
                    message: format!(
                        "Expected update hunk to start with a @@ context marker, got: '{line}'"
                    ),
                    line_number: self.line_number,
                });
            }
        }
        if chunks.is_empty()
            && move_path.is_none()
            && let Some(destination) = update_line.strip_prefix(MOVE_TO_MARKER)
        {
            *move_path = Some(destination.to_string());
            return Ok(());
        }
        if is_context_marker && last_chunk_is_empty {
            return Err(ParseError::InvalidHunk {
                message: format!("Unexpected line found in update hunk: '{line}'. {LINE_PREFIXES}"),
                line_number: self.line_number,
            });
        }
        if update_line == EMPTY_CHANGE_CONTEXT_MARKER {
            chunks.push(UpdateFileChunk::default());
            return Ok(());
        }
        if let Some(change_context) = update_line.strip_prefix(CHANGE_CONTEXT_MARKER) {
            chunks.push(UpdateFileChunk {
                change_context: Some(change_context.to_string()),
                ..UpdateFileChunk::default()
            });
            return Ok(());
        }
        if update_line == EOF_MARKER {
            if last_chunk_is_empty {
                return Err(ParseError::InvalidHunk {
                    message: "Update hunk does not contain any lines".to_string(),
                    line_number: self.line_number,
                });
            }
            if let Some(chunk) = chunks.last_mut() {
                chunk.is_end_of_file = true;
            }
            return Ok(());
        }

        let (marker, content) = match line.chars().next() {
            None => (' ', ""),
            Some(first) if matches!(first, ' ' | '+' | '-') => (first, &line[1..]),
            Some(_) => {
                let message = if last_chunk_is_empty || chunks.is_empty() {
                    format!("Unexpected line found in update hunk: '{line}'. {LINE_PREFIXES}")
                } else {
                    format!("Expected update hunk to start with a @@ context marker, got: '{line}'")
                };
                return Err(ParseError::InvalidHunk {
                    message,
                    line_number: self.line_number,
                });
            }
        };
        if chunks.is_empty() {
            chunks.push(UpdateFileChunk::default());
        }
        let Some(chunk) = chunks.last_mut() else {
            return Ok(());
        };
        match marker {
            '+' => chunk.new_lines.push(content.to_string()),
            '-' => chunk.old_lines.push(content.to_string()),
            _ => {
                chunk.old_lines.push(content.to_string());
                chunk.new_lines.push(content.to_string());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(context: Option<&str>, old: &[&str], new: &[&str], eof: bool) -> UpdateFileChunk {
        UpdateFileChunk {
            change_context: context.map(str::to_string),
            old_lines: old.iter().map(|s| (*s).to_string()).collect(),
            new_lines: new.iter().map(|s| (*s).to_string()).collect(),
            is_end_of_file: eof,
        }
    }

    #[test]
    fn envelope_errors_name_the_missing_marker() {
        assert_eq!(
            parse_patch("bad"),
            Err(ParseError::InvalidPatch(
                "The first line of the patch must be '*** Begin Patch'".to_string()
            ))
        );
        assert_eq!(
            parse_patch("*** Begin Patch\nbad"),
            Err(ParseError::InvalidPatch(
                "The last line of the patch must be '*** End Patch'".to_string()
            ))
        );
        // Content after the end marker is an envelope error too.
        assert_eq!(
            parse_patch(
                "*** Begin Patch\n*** Add File: f\n+x\n*** End Patch\nextra\n*** End Patch"
            ),
            Err(ParseError::InvalidPatch(
                "The last line of the patch must be '*** End Patch'".to_string()
            ))
        );
        assert_eq!(
            parse_patch("*** Begin Patch\n*** End Patch"),
            Ok(Vec::new())
        );
    }

    #[test]
    fn markers_tolerate_surrounding_whitespace() {
        // Codex fixtures 017, 018 and 020.
        let hunks = parse_patch(
            " *** Begin Patch \n  *** Update File: foo.txt\n@@\n-old\n+new\n *** End Patch ",
        )
        .expect("parses");
        assert_eq!(
            hunks,
            vec![Hunk::UpdateFile {
                path: "foo.txt".to_string(),
                move_path: None,
                chunks: vec![chunk(None, &["old"], &["new"], false)],
            }]
        );
        assert_eq!(
            parse_patch("*** Begin Patch \n*** Add File: foo\n+hi\n *** End Patch"),
            Ok(vec![Hunk::AddFile {
                path: "foo".to_string(),
                contents: "hi\n".to_string()
            }])
        );
    }

    #[test]
    fn every_operation_in_one_patch() {
        let hunks = parse_patch(
            "*** Begin Patch\n\
             *** Add File: path/add.py\n\
             +abc\n\
             +def\n\
             *** Delete File: path/delete.py\n\
             *** Update File: path/update.py\n\
             *** Move to: path/update2.py\n\
             @@ def f():\n\
             -    pass\n\
             +    return 123\n\
             *** End Patch",
        )
        .expect("parses");
        assert_eq!(
            hunks,
            vec![
                Hunk::AddFile {
                    path: "path/add.py".to_string(),
                    contents: "abc\ndef\n".to_string(),
                },
                Hunk::DeleteFile {
                    path: "path/delete.py".to_string(),
                },
                Hunk::UpdateFile {
                    path: "path/update.py".to_string(),
                    move_path: Some("path/update2.py".to_string()),
                    chunks: vec![chunk(
                        Some("def f():"),
                        &["    pass"],
                        &["    return 123"],
                        false
                    )],
                },
            ]
        );
        assert_eq!(
            hunks[2].path(),
            "path/update2.py",
            "a rename reports its destination"
        );
    }

    #[test]
    fn first_chunk_may_omit_the_context_marker() {
        let hunks = parse_patch(
            "*** Begin Patch\n*** Update File: file2.py\n import foo\n+bar\n*** End Patch",
        )
        .expect("parses");
        assert_eq!(
            hunks,
            vec![Hunk::UpdateFile {
                path: "file2.py".to_string(),
                move_path: None,
                chunks: vec![chunk(None, &["import foo"], &["import foo", "bar"], false)],
            }]
        );
    }

    #[test]
    fn end_of_file_marker_pins_the_chunk() {
        let hunks = parse_patch(
            "*** Begin Patch\n*** Update File: file.txt\n@@\n+quux\n*** End of File\n\n*** End Patch",
        )
        .expect("parses");
        assert_eq!(
            hunks,
            vec![Hunk::UpdateFile {
                path: "file.txt".to_string(),
                move_path: None,
                chunks: vec![chunk(None, &[], &["quux"], true)],
            }]
        );
    }

    #[test]
    fn indented_markers_inside_an_update_are_context_lines() {
        let hunks = parse_patch(
            "*** Begin Patch\n*** Update File: a.txt\n@@\n-old a\n+new a\n *** Update File: b.txt\n@@\n-old b\n+new b\n*** End Patch",
        )
        .expect("parses");
        assert_eq!(
            hunks,
            vec![Hunk::UpdateFile {
                path: "a.txt".to_string(),
                move_path: None,
                chunks: vec![
                    chunk(
                        None,
                        &["old a", "*** Update File: b.txt"],
                        &["new a", "*** Update File: b.txt"],
                        false
                    ),
                    chunk(None, &["old b"], &["new b"], false),
                ],
            }]
        );
    }

    #[test]
    fn bare_empty_lines_are_empty_context_lines() {
        let hunks = parse_patch(
            "*** Begin Patch\n*** Update File: file.txt\n@@\n context before\n\n context after\n*** End Patch",
        )
        .expect("parses");
        assert_eq!(
            hunks,
            vec![Hunk::UpdateFile {
                path: "file.txt".to_string(),
                move_path: None,
                chunks: vec![chunk(
                    None,
                    &["context before", "", "context after"],
                    &["context before", "", "context after"],
                    false
                )],
            }]
        );
    }

    #[test]
    fn crlf_patches_parse_like_lf_ones() {
        let hunks = parse_patch(
            "*** Begin Patch\r\n*** Update File: file.txt\r\n@@\r\n-old\r\n+new\r\n*** End Patch\r\n",
        )
        .expect("parses");
        assert_eq!(
            hunks,
            vec![Hunk::UpdateFile {
                path: "file.txt".to_string(),
                move_path: None,
                chunks: vec![chunk(None, &["old"], &["new"], false)],
            }]
        );
    }

    #[test]
    fn heredoc_wrapper_is_stripped() {
        let body = "*** Begin Patch\n*** Update File: file2.py\n import foo\n+bar\n*** End Patch";
        let expected = vec![Hunk::UpdateFile {
            path: "file2.py".to_string(),
            move_path: None,
            chunks: vec![chunk(None, &["import foo"], &["import foo", "bar"], false)],
        }];
        for wrapper in ["<<EOF", "<<'EOF'", "<<\"EOF\""] {
            assert_eq!(
                parse_patch(&format!("{wrapper}\n{body}\nEOF\n")),
                Ok(expected.clone())
            );
        }
        // Mismatched quotes are not a heredoc; the strict error stands.
        assert_eq!(
            parse_patch(&format!("<<\"EOF'\n{body}\nEOF\n")),
            Err(ParseError::InvalidPatch(
                "The first line of the patch must be '*** Begin Patch'".to_string()
            ))
        );
        assert_eq!(
            parse_patch("<<EOF\n*** Begin Patch\n*** Update File: file2.py\nEOF\n"),
            Err(ParseError::InvalidPatch(
                "The last line of the patch must be '*** End Patch'".to_string()
            ))
        );
    }

    #[test]
    fn hunk_errors_carry_codex_messages_and_line_numbers() {
        let cases: &[(&str, &str, usize)] = &[
            (
                "*** Begin Patch\nbad\n*** End Patch",
                "'bad' is not a valid hunk header. Valid hunk headers: '*** Add File: {path}', '*** Delete File: {path}', '*** Update File: {path}'",
                2,
            ),
            (
                "*** Begin Patch\n*** Frobnicate File: foo\n*** End Patch",
                "'*** Frobnicate File: foo' is not a valid hunk header. Valid hunk headers: '*** Add File: {path}', '*** Delete File: {path}', '*** Update File: {path}'",
                2,
            ),
            (
                "*** Begin Patch\n*** Add File: file.txt\nbad\n*** End Patch",
                "'bad' is not a valid hunk header. Valid hunk headers: '*** Add File: {path}', '*** Delete File: {path}', '*** Update File: {path}'",
                3,
            ),
            (
                "*** Begin Patch\n*** Delete File: file.txt\nbad\n*** End Patch",
                "'bad' is not a valid hunk header. Valid hunk headers: '*** Add File: {path}', '*** Delete File: {path}', '*** Update File: {path}'",
                3,
            ),
            (
                "*** Begin Patch\n*** Update File: file.txt\n*** End Patch",
                "Update file hunk for path 'file.txt' is empty",
                2,
            ),
            (
                "*** Begin Patch\n*** Update File: old.txt\n*** Move to: new.txt\n*** Delete File: other.txt\n*** End Patch",
                "Update file hunk for path 'old.txt' is empty",
                2,
            ),
            (
                "*** Begin Patch\n*** Update File: file.txt\n@@\n*** End Patch",
                "Update hunk does not contain any lines",
                4,
            ),
            (
                "*** Begin Patch\n*** Update File: file.txt\n@@\n*** End of File\n*** End Patch",
                "Update hunk does not contain any lines",
                4,
            ),
            (
                "*** Begin Patch\n*** Update File: file.txt\n@@\n@@\n*** End Patch",
                "Unexpected line found in update hunk: '@@'. Every line should start with ' ' (context line), '+' (added line), or '-' (removed line)",
                4,
            ),
            (
                "*** Begin Patch\n*** Update File: file.txt\n@@\n-old\nbad\n*** End Patch",
                "Expected update hunk to start with a @@ context marker, got: 'bad'",
                5,
            ),
            (
                "*** Begin Patch\n*** Update File: file.txt\n@@\n*** Update File: other.txt\n*** End Patch",
                "Unexpected line found in update hunk: '*** Update File: other.txt'. Every line should start with ' ' (context line), '+' (added line), or '-' (removed line)",
                4,
            ),
            (
                "*** Begin Patch\n*** Update File: file.txt\n@@\n+x\n*** End of File\n-y\n*** End Patch",
                "Expected update hunk to start with a @@ context marker, got: '-y'",
                6,
            ),
        ];
        for (patch, message, line_number) in cases {
            assert_eq!(
                parse_patch(patch),
                Err(ParseError::InvalidHunk {
                    message: (*message).to_string(),
                    line_number: *line_number,
                }),
                "patch:\n{patch}"
            );
        }
    }

    #[test]
    fn errors_display_like_codex() {
        let error = parse_patch("*** Begin Patch\n*** Update File: f\n*** End Patch").unwrap_err();
        assert_eq!(
            error.to_string(),
            "invalid hunk at line 2, Update file hunk for path 'f' is empty"
        );
        assert_eq!(
            parse_patch("nope").unwrap_err().to_string(),
            "invalid patch: The first line of the patch must be '*** Begin Patch'"
        );
    }
}
