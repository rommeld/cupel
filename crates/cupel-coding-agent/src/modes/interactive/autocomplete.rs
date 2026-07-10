//! File-reference autocomplete: the state machine behind the `@path` popup.
//! Port of the file-completion half of pi's `tui/src/autocomplete.ts`.
//!
//! Life of a session: the user types `@` at a token start ->
//! [`Autocomplete::refresh`] finds the token, walks the project tree once
//! (bounded), and fuzzy-filters it -> every subsequent keystroke re-filters
//! the cached walk -> accepting a directory re-walks one level deeper and
//! keeps completing; accepting a file inserts `@path ` and closes.
//!
//! Deviations from pi, both deliberate: the file list comes from the
//! `ignore` crate instead of shelling out to the `fd` binary (cupel links
//! its search engines - same policy as grep), and instead of re-running the
//! walk per keystroke we walk once per directory prefix and filter the
//! cached list live (a subprocess per keystroke is idiomatic Node,
//! wasteful in-process).

use std::path::{Path, PathBuf};

use super::fuzzy::fuzzy_filter;

/// How many entries the project walk collects. Beyond this, deeper files
/// become reachable by typing a directory prefix (which re-roots the walk).
const WALK_CAP: usize = 1000;
/// Rows shown in the popup.
pub const MAX_VISIBLE: usize = 8;

// ---------------------------------------------------------------------------
// Token extraction
// ---------------------------------------------------------------------------

/// The `@`-token under construction at the cursor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileToken {
    /// CHAR index of the `@` in the buffer.
    pub start: usize,
    /// Text between `@` (or `@"`) and the cursor - the fuzzy query.
    pub query: String,
    /// Opened as `@"` (the accepted path will be quoted).
    pub quoted: bool,
}

/// Find the `@`-token the cursor is currently inside, if any.
///
/// Rules (pi's): the `@` must sit at a token start - beginning of text or
/// right after whitespace (including newlines, so tokens never span lines).
/// Only text BEFORE the cursor forms the query; `user@host` never triggers
/// because its `@` follows a non-space character. Quoted tokens (`@"...`)
/// may contain spaces and stay open until the closing quote.
///
/// A quoted token means a simple stop-at-whitespace backward scan can't
/// work: in `@"my file`, the space is INSIDE the token, but a backward scan
/// from the cursor hits it before ever seeing the opening quote. Instead,
/// every `@` before the cursor is examined nearest-first, and the segment
/// between it and the cursor decides validity.
#[must_use]
pub fn file_token_at_cursor(text: &str, cursor: usize) -> Option<FileToken> {
    let chars: Vec<char> = text.chars().collect();
    let cursor = cursor.min(chars.len());

    for at in (0..cursor).rev().filter(|i| chars[*i] == '@') {
        let at_token_start = at == 0 || chars[at - 1].is_whitespace();
        if !at_token_start {
            continue; // e.g. the `@` in user@host; an outer `@"a@b` still wins
        }
        let quoted = chars.get(at + 1) == Some(&'"');
        let query_start = (if quoted { at + 2 } else { at + 1 }).min(cursor);
        let segment = &chars[query_start..cursor];

        let valid = if quoted {
            // Open until the closing quote; any quote in the segment means
            // the token already closed before the cursor.
            !segment.contains(&'"')
        } else {
            // Unquoted tokens end at whitespace (or a quote).
            !segment.iter().any(|c| c.is_whitespace() || *c == '"')
        };
        return valid.then(|| FileToken {
            start: at,
            query: segment.iter().collect(),
            quoted,
        });
    }
    None
}

// ---------------------------------------------------------------------------
// File enumeration
// ---------------------------------------------------------------------------

/// One completable entry. `display` is the path relative to the project
/// root; directories carry a trailing `/` so they read (and complete) as
/// prefixes.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub display: String,
    pub is_dir: bool,
}

/// Bounded, gitignore-aware walk - the same knobs as the grep backend
/// (hidden files in, `.git` out) plus followed symlinks, matching pi's fd
/// invocation. `prefix` re-roots the walk for directory drill-down while
/// keeping displays relative to the project root.
#[must_use]
pub fn list_candidates(root: &Path, prefix: &str, cap: usize) -> Vec<Candidate> {
    let walk_root = if prefix.is_empty() {
        root.to_path_buf()
    } else {
        root.join(prefix)
    };
    let mut out = Vec::new();
    let walker = ignore::WalkBuilder::new(&walk_root)
        .hidden(false) // do descend into dotfiles (.github, .cupel, ...)
        .follow_links(true)
        .filter_entry(|entry| entry.file_name() != ".git")
        .build();

    for entry in walker.flatten() {
        if out.len() >= cap {
            break;
        }
        let Ok(relative) = entry.path().strip_prefix(&walk_root) else {
            continue;
        };
        if relative.as_os_str().is_empty() {
            continue; // the walk root itself
        }
        let is_dir = entry.file_type().is_some_and(|t| t.is_dir());
        // Non-UTF-8 names render lossily; such a path may not round-trip
        // into a tool call, which is acceptable for a completion hint.
        let mut display = format!("{prefix}{}", relative.display());
        if is_dir {
            display.push('/');
        }
        out.push(Candidate { display, is_dir });
    }
    out
}

// ---------------------------------------------------------------------------
// The session state machine
// ---------------------------------------------------------------------------

/// What accepting the selected row does to the input buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Completion {
    /// CHAR range in the buffer to replace (the whole `@...` token up to
    /// the cursor).
    pub start: usize,
    pub end: usize,
    pub insert: String,
    /// Directories keep the session open for drill-down.
    pub is_dir: bool,
}

struct Session {
    token: FileToken,
    /// The directory prefix the cached walk was rooted at.
    walked_prefix: String,
    candidates: Vec<Candidate>,
    /// Top [`MAX_VISIBLE`] fuzzy matches for the current query.
    matches: Vec<Candidate>,
    selected: usize,
}

/// Owned by the `App`; consulted from key handling and the render pass.
pub struct Autocomplete {
    root: PathBuf,
    session: Option<Session>,
}

impl Autocomplete {
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            session: None,
        }
    }

    #[must_use]
    pub fn is_open(&self) -> bool {
        self.session.is_some()
    }

    pub fn close(&mut self) {
        self.session = None;
    }

    /// Rows to draw plus the selected index, or `None` while closed/empty.
    #[must_use]
    pub fn visible(&self) -> Option<(&[Candidate], usize)> {
        let session = self.session.as_ref()?;
        (!session.matches.is_empty()).then_some(((&*session.matches), session.selected))
    }

    /// CHAR index of the token's `@` (for anchoring the popup).
    #[must_use]
    pub fn token_start(&self) -> Option<usize> {
        self.session.as_ref().map(|s| s.token.start)
    }

    pub fn move_up(&mut self) {
        if let Some(session) = &mut self.session {
            session.selected = session.selected.saturating_sub(1);
        }
    }

    pub fn move_down(&mut self) {
        if let Some(session) = &mut self.session {
            let last = session.matches.len().saturating_sub(1);
            session.selected = (session.selected + 1).min(last);
        }
    }

    /// Recompute from the current buffer state: opens a session when the
    /// cursor sits in an `@`-token, re-filters while it does, closes when
    /// it no longer does (e.g. backspacing past the `@`).
    pub fn refresh(&mut self, text: &str, cursor: usize) {
        let Some(token) = file_token_at_cursor(text, cursor) else {
            self.session = None;
            return;
        };

        // The query's directory part decides the walk root: typing `src/`
        // (or accepting the `src/` completion) re-roots the walk there,
        // making files beyond the cap reachable by drilling down.
        let prefix = token
            .query
            .rfind('/')
            .map_or(String::new(), |slash| token.query[..=slash].to_string());

        let needs_walk = match &self.session {
            Some(session) => session.walked_prefix != prefix,
            None => true,
        };
        if needs_walk {
            let candidates = list_candidates(&self.root, &prefix, WALK_CAP);
            self.session = Some(Session {
                token: token.clone(),
                walked_prefix: prefix,
                candidates,
                matches: Vec::new(),
                selected: 0,
            });
        }

        let session = self.session.as_mut().expect("session ensured above");
        session.token = token;
        session.matches = fuzzy_filter(&session.token.query, &session.candidates, |candidate| {
            &candidate.display
        })
        .into_iter()
        .take(MAX_VISIBLE)
        .cloned()
        .collect();
        session.selected = session
            .selected
            .min(session.matches.len().saturating_sub(1));
    }

    /// Completion for the selected row. The session itself is closed (or
    /// kept, for directories) by the follow-up `refresh` after the caller
    /// applies the edit.
    #[must_use]
    pub fn accept(&self, cursor: usize) -> Option<Completion> {
        let session = self.session.as_ref()?;
        let candidate = session.matches.get(session.selected)?;

        // Quote when the path demands it or the token was opened quoted.
        let needs_quotes = session.token.quoted || candidate.display.contains(' ');
        let mut insert = if needs_quotes && !candidate.is_dir {
            format!("@\"{}\"", candidate.display)
        } else if needs_quotes {
            // Directory in quote mode: leave the quote open so the session
            // keeps completing inside it.
            format!("@\"{}", candidate.display)
        } else {
            format!("@{}", candidate.display)
        };
        // A trailing space after a FILE lets typing resume naturally (pi
        // does the same); directories keep completing instead.
        if !candidate.is_dir {
            insert.push(' ');
        }

        Some(Completion {
            start: session.token.start,
            end: cursor,
            insert,
            is_dir: candidate.is_dir,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- token extraction ---------------------------------------------------

    fn token(text: &str) -> Option<FileToken> {
        file_token_at_cursor(text, text.chars().count())
    }

    #[test]
    fn at_token_detected_at_text_start_and_after_whitespace() {
        assert_eq!(
            token("@src/ma"),
            Some(FileToken {
                start: 0,
                query: "src/ma".into(),
                quoted: false
            })
        );
        assert_eq!(token("fix @lib").map(|t| t.start), Some(4));
        assert_eq!(token("line one\n@x").map(|t| t.start), Some(9));
    }

    #[test]
    fn email_like_at_does_not_trigger() {
        assert_eq!(token("mail user@host about it"), None);
        assert_eq!(token("user@ho"), None);
    }

    #[test]
    fn quoted_token_captures_spaces() {
        let t = token("see @\"my file").expect("quoted token");
        assert!(t.quoted);
        assert_eq!(t.query, "my file");
        // A closed quote ends the token.
        assert_eq!(token("see @\"my file\" and"), None);
    }

    #[test]
    fn cursor_mid_token_uses_prefix_only() {
        // Cursor after "sr" inside "@src".
        let t = file_token_at_cursor("@src", 3).expect("token");
        assert_eq!(t.query, "sr");
    }

    #[test]
    fn whitespace_ends_unquoted_tokens() {
        assert_eq!(token("@src stuff"), None);
    }

    // ---- walking -------------------------------------------------------------

    fn temp_tree(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("cupel-autocomplete-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() {}").unwrap();
        std::fs::write(root.join("README.md"), "# hi").unwrap();
        std::fs::write(root.join(".env"), "SECRET=1").unwrap();
        std::fs::write(root.join(".gitignore"), "ignored.txt\n").unwrap();
        std::fs::write(root.join("ignored.txt"), "x").unwrap();
        std::fs::write(root.join(".git/config"), "x").unwrap();
        root
    }

    #[test]
    fn walk_respects_gitignore_includes_hidden_excludes_git() {
        let root = temp_tree("walk");
        let names: Vec<String> = list_candidates(&root, "", 100)
            .into_iter()
            .map(|c| c.display)
            .collect();
        assert!(names.contains(&"src/".to_string()), "{names:?}");
        assert!(names.contains(&"src/main.rs".to_string()));
        assert!(names.contains(&".env".to_string()), "hidden files included");
        assert!(!names.iter().any(|n| n.starts_with(".git/")), "{names:?}");
        assert!(!names.contains(&"ignored.txt".to_string()), "gitignore");
    }

    #[test]
    fn walk_cap_truncates() {
        let root = temp_tree("cap");
        assert_eq!(list_candidates(&root, "", 2).len(), 2);
    }

    #[test]
    fn prefixed_walk_keeps_root_relative_displays() {
        let root = temp_tree("prefix");
        let names: Vec<String> = list_candidates(&root, "src/", 100)
            .into_iter()
            .map(|c| c.display)
            .collect();
        assert_eq!(names, vec!["src/main.rs".to_string()]);
    }

    // ---- session lifecycle -----------------------------------------------------

    #[test]
    fn session_opens_narrows_and_closes() {
        let root = temp_tree("session");
        let mut ac = Autocomplete::new(&root);

        ac.refresh("@", 1);
        assert!(ac.is_open());
        let (rows, _) = ac.visible().expect("rows");
        assert!(!rows.is_empty());

        ac.refresh("@mai", 4);
        let (rows, selected) = ac.visible().expect("rows");
        assert_eq!(rows[selected].display, "src/main.rs");

        // Backspacing past the `@` closes the session.
        ac.refresh("", 0);
        assert!(!ac.is_open());
    }

    #[test]
    fn accepting_a_file_inserts_reference_with_trailing_space() {
        let root = temp_tree("accept");
        let mut ac = Autocomplete::new(&root);
        ac.refresh("@mai", 4);
        let completion = ac.accept(4).expect("completion");
        assert_eq!(completion.start, 0);
        assert_eq!(completion.end, 4);
        assert_eq!(completion.insert, "@src/main.rs ");
        assert!(!completion.is_dir);
    }

    #[test]
    fn accepting_a_directory_drills_down() {
        let root = temp_tree("drill");
        let mut ac = Autocomplete::new(&root);
        ac.refresh("@src/", 5); // as if `@src/` was just accepted/typed
        assert!(ac.is_open(), "directory prefix keeps the session open");
        let (rows, _) = ac.visible().expect("rows");
        assert_eq!(rows[0].display, "src/main.rs");
    }

    #[test]
    fn paths_with_spaces_are_quoted() {
        let root = temp_tree("quote");
        std::fs::write(root.join("my notes.md"), "x").unwrap();
        let mut ac = Autocomplete::new(&root);
        ac.refresh("@notes", 6);
        let completion = ac.accept(6).expect("completion");
        assert_eq!(completion.insert, "@\"my notes.md\" ");
    }
}
