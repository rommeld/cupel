//! Code search abstraction + the grep-based default backend.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use grep_searcher::sinks::UTF8;
use grep_searcher::{BinaryDetection, SearcherBuilder};
use ignore::WalkBuilder;
use ignore::overrides::OverrideBuilder;
use tokio_util::sync::CancellationToken;

/// A search request. Field-for-field mirror of pi's grep tool input.
#[derive(Debug, Clone)]
pub struct SearchQuery {
    /// Regex (or literal string when `literal` is set).
    pub pattern: String,
    /// Directory or file to search; relative to the search root.
    pub path: Option<String>,
    /// Glob filter, e.g. `*.rs` or `**/*.spec.ts`.
    pub glob: Option<String>,
    pub ignore_case: bool,
    /// Treat `pattern` as a literal string instead of a regex.
    pub literal: bool,
    /// Stop after this many matches.
    pub limit: usize,
}

/// One matching line.
#[derive(Debug, Clone)]
pub struct SearchMatch {
    /// Absolute path of the file.
    pub path: PathBuf,
    /// 1-based line number.
    pub line_number: u64,
    /// The matching line's text (trailing newline stripped).
    pub line: String,
}

/// Result of a search.
#[derive(Debug, Clone, Default)]
pub struct SearchOutcome {
    pub matches: Vec<SearchMatch>,
    /// True when the match limit cut the search short.
    pub limit_reached: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    #[error("Path not found: {0}")]
    PathNotFound(String),
    #[error("Invalid pattern: {0}")]
    InvalidPattern(String),
    #[error("Invalid glob: {0}")]
    InvalidGlob(String),
    #[error("Operation aborted")]
    Aborted,
    #[error("Search failed: {0}")]
    Io(#[from] std::io::Error),
}

/// The backend interface the grep tool is written against.
///
/// Iteration two adds an index-backed implementation in `cupel-index`;
/// see the module docs for why this indirection exists.
#[async_trait::async_trait]
pub trait CodeSearch: Send + Sync {
    async fn search(
        &self,
        query: SearchQuery,
        cancel: CancellationToken,
    ) -> Result<SearchOutcome, SearchError>;
}

/// Resolve a query path against the search root: absolute paths pass
/// through, `~` expands, everything else is joined onto the root.
#[must_use]
pub fn resolve_to_root(path: &str, root: &Path) -> PathBuf {
    // pi's stripAtPrefix: prompts reference files as `@path`, and models
    // sometimes echo that convention verbatim into tool calls
    // (read("@src/main.rs")). Tolerate exactly one leading `@`.
    let path = path.strip_prefix('@').unwrap_or(path);
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = std::env::home_dir()
    {
        return home.join(rest);
    }
    let candidate = Path::new(path);
    if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        root.join(candidate)
    }
}

// ---------------------------------------------------------------------------
// Grep backend
// ---------------------------------------------------------------------------

/// File-scan search backend using ripgrep's engine. Semantics match pi's
/// `rg --json --line-number --hidden` invocation: respects `.gitignore`,
/// includes hidden files (but never the `.git` directory itself).
pub struct GrepSearch {
    /// Root directory relative paths resolve against (the agent's cwd).
    root: PathBuf,
}

impl GrepSearch {
    #[must_use]
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
}

#[async_trait::async_trait]
impl CodeSearch for GrepSearch {
    async fn search(
        &self,
        query: SearchQuery,
        cancel: CancellationToken,
    ) -> Result<SearchOutcome, SearchError> {
        let root = self.root.clone();
        // The grep/ignore crates are synchronous and CPU/IO heavy. Running
        // them on `spawn_blocking` keeps the async runtime's worker threads
        // free - blocking inside an async fn would stall unrelated tasks.
        tokio::task::spawn_blocking(move || search_blocking(&root, &query, &cancel))
            .await
            .map_err(|e| SearchError::Io(std::io::Error::other(e)))?
    }
}

/// Escape regex metacharacters so the pattern matches literally
/// (rg's `--fixed-strings`).
fn escape_regex(pattern: &str) -> String {
    let mut escaped = String::with_capacity(pattern.len() * 2);
    for c in pattern.chars() {
        if matches!(
            c,
            '\\' | '.'
                | '+'
                | '*'
                | '?'
                | '('
                | ')'
                | '|'
                | '['
                | ']'
                | '{'
                | '}'
                | '^'
                | '$'
                | '#'
                | '&'
                | '-'
                | '~'
        ) {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped
}

fn search_blocking(
    root: &Path,
    query: &SearchQuery,
    cancel: &CancellationToken,
) -> Result<SearchOutcome, SearchError> {
    let search_path = resolve_to_root(query.path.as_deref().unwrap_or("."), root);
    if !search_path.exists() {
        return Err(SearchError::PathNotFound(search_path.display().to_string()));
    }

    // ---- Matcher ----------------------------------------------------------
    let pattern = if query.literal {
        escape_regex(&query.pattern)
    } else {
        query.pattern.clone()
    };
    let matcher = grep_regex::RegexMatcherBuilder::new()
        .case_insensitive(query.ignore_case)
        .build(&pattern)
        .map_err(|e| SearchError::InvalidPattern(e.to_string()))?;

    // ---- Walker -----------------------------------------------------------
    let mut walker = WalkBuilder::new(&search_path);
    walker
        // Include hidden files (rg --hidden) ...
        .hidden(false)
        // ... but keep .gitignore/.ignore rules active (the default).
        .git_ignore(true)
        .git_exclude(true)
        // Deviation from `rg --hidden`, which would descend into `.git`:
        // matches inside git internals are never what a coding agent wants.
        .filter_entry(|entry| entry.file_name() != ".git");

    if let Some(glob) = &query.glob {
        let mut overrides = OverrideBuilder::new(&search_path);
        overrides
            .add(glob)
            .map_err(|e| SearchError::InvalidGlob(e.to_string()))?;
        walker.overrides(
            overrides
                .build()
                .map_err(|e| SearchError::InvalidGlob(e.to_string()))?,
        );
    }

    // ---- Searcher ---------------------------------------------------------
    let mut searcher = SearcherBuilder::new()
        .line_number(true)
        // Skip binary files as soon as a NUL byte appears (rg's default).
        .binary_detection(BinaryDetection::quit(b'\x00'))
        .build();

    let mut matches: Vec<SearchMatch> = Vec::new();
    let limit = query.limit.max(1);
    // Shared counter lets the per-file sink stop the whole search at the
    // limit. Atomic because sink closures can't borrow `matches` mutably
    // while the outer loop also does.
    let count = Arc::new(AtomicUsize::new(0));
    let mut limit_reached = false;

    // Single-threaded walk => deterministic result order (like rg's
    // single-threaded default when outputting to a pipe).
    for entry in walker.build() {
        if cancel.is_cancelled() {
            return Err(SearchError::Aborted);
        }
        if count.load(Ordering::Relaxed) >= limit {
            limit_reached = true;
            break;
        }

        let Ok(entry) = entry else { continue };
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }

        let path = entry.path().to_path_buf();
        let count_for_sink = Arc::clone(&count);
        let sink_path = path.clone();
        let mut file_matches: Vec<SearchMatch> = Vec::new();

        let result = searcher.search_path(
            &matcher,
            &path,
            UTF8(|line_number, line| {
                file_matches.push(SearchMatch {
                    path: sink_path.clone(),
                    line_number,
                    line: line.trim_end_matches(['\r', '\n']).to_string(),
                });
                let seen = count_for_sink.fetch_add(1, Ordering::Relaxed) + 1;
                // Returning Ok(false) stops the search in THIS file.
                Ok(seen < limit)
            }),
        );
        // Unreadable files are skipped silently, matching rg's behavior of
        // printing a warning and moving on.
        if result.is_err() {
            continue;
        }
        matches.extend(file_matches);
    }

    if count.load(Ordering::Relaxed) >= limit {
        limit_reached = true;
        matches.truncate(limit);
    }

    Ok(SearchOutcome {
        matches,
        limit_reached,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_to_root_strips_one_at_prefix() {
        let root = Path::new("/project");
        // `@path` references from prompts resolve like plain paths.
        assert_eq!(
            resolve_to_root("@src/main.rs", root),
            PathBuf::from("/project/src/main.rs")
        );
        assert_eq!(resolve_to_root("@/abs/x", root), PathBuf::from("/abs/x"));
        // Plain paths are untouched; only ONE @ is stripped (a literal
        // `@@weird` file stays reachable as `@@weird` -> `@weird`... rare
        // enough that pi accepts the same trade).
        assert_eq!(
            resolve_to_root("src/main.rs", root),
            PathBuf::from("/project/src/main.rs")
        );
        assert_eq!(
            resolve_to_root("@@weird", root),
            PathBuf::from("/project/@weird")
        );
    }

    fn write_tree(dir: &Path, files: &[(&str, &str)]) {
        for (name, content) in files {
            let path = dir.join(name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(path, content).unwrap();
        }
    }

    fn temp_root(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("cupel-grep-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn run(root: &Path, query: SearchQuery) -> SearchOutcome {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(GrepSearch::new(root).search(query, CancellationToken::new()))
            .unwrap()
    }

    fn base_query(pattern: &str) -> SearchQuery {
        SearchQuery {
            pattern: pattern.to_string(),
            path: None,
            glob: None,
            ignore_case: false,
            literal: false,
            limit: 100,
        }
    }

    #[test]
    fn finds_matches_with_line_numbers() {
        let root = temp_root("basic");
        write_tree(&root, &[("a.txt", "hello\nworld\nhello again\n")]);
        let outcome = run(&root, base_query("hello"));
        assert_eq!(outcome.matches.len(), 2);
        assert_eq!(outcome.matches[0].line_number, 1);
        assert_eq!(outcome.matches[1].line_number, 3);
        assert_eq!(outcome.matches[1].line, "hello again");
    }

    #[test]
    fn respects_gitignore() {
        let root = temp_root("gitignore");
        write_tree(
            &root,
            &[
                (".gitignore", "ignored.txt\n"),
                ("ignored.txt", "secret\n"),
                ("kept.txt", "secret\n"),
            ],
        );
        // .gitignore applies to repositories; init a fake .git dir.
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let outcome = run(&root, base_query("secret"));
        assert_eq!(outcome.matches.len(), 1);
        assert!(outcome.matches[0].path.ends_with("kept.txt"));
    }

    #[test]
    fn literal_mode_escapes_regex() {
        let root = temp_root("literal");
        write_tree(&root, &[("a.txt", "price is $5.00\nnot a match\n")]);
        let mut query = base_query("$5.00");
        query.literal = true;
        let outcome = run(&root, query);
        assert_eq!(outcome.matches.len(), 1);
    }

    #[test]
    fn glob_filters_files() {
        let root = temp_root("glob");
        write_tree(&root, &[("a.rs", "target\n"), ("a.txt", "target\n")]);
        let mut query = base_query("target");
        query.glob = Some("*.rs".to_string());
        let outcome = run(&root, query);
        assert_eq!(outcome.matches.len(), 1);
        assert!(outcome.matches[0].path.ends_with("a.rs"));
    }

    #[test]
    fn limit_stops_search() {
        let root = temp_root("limit");
        write_tree(&root, &[("a.txt", "x\nx\nx\nx\nx\n")]);
        let mut query = base_query("x");
        query.limit = 2;
        let outcome = run(&root, query);
        assert_eq!(outcome.matches.len(), 2);
        assert!(outcome.limit_reached);
    }
}
