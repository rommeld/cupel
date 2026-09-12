//! Code search abstraction + the grep-based default backend.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use grep_searcher::sinks::UTF8;
use grep_searcher::{BinaryDetection, SearcherBuilder};
use ignore::WalkBuilder;
use ignore::overrides::OverrideBuilder;
use tokio_util::sync::CancellationToken;

#[derive(Debug, Clone)]
pub struct SearchQuery {
    pub pattern: String,
    pub path: Option<String>,
    pub glob: Option<String>,
    pub ignore_case: bool,
    pub literal: bool,
    pub limit: SearchLimit,
}

/// How much of the search to run before stopping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchLimit {
    Matches(usize),
    Files { max_files: usize, per_file: usize },
}

/// One matching line.
#[derive(Debug, Clone)]
pub struct SearchMatch {
    pub path: PathBuf,
    pub line_number: u64,
    pub line: String,
}

/// Result of a search.
#[derive(Debug, Clone, Default)]
pub struct SearchOutcome {
    pub matches: Vec<SearchMatch>,
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

/// File-scan search backend using ripgrep's engine. Semantics match pi's
/// `rg --json --line-number --hidden` invocation: respects `.gitignore`,
/// includes hidden files (but never the `.git` directory itself).
pub struct GrepSearch {
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
        // free.
        tokio::task::spawn_blocking(move || search_blocking(&root, &query, &cancel))
            .await
            .map_err(|e| SearchError::Io(std::io::Error::other(e)))?
    }
}

/// Escape regex metacharacters so the pattern matches literally
/// (rg's `--fixed-strings`).
pub(crate) fn escape_regex(pattern: &str) -> String {
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

    let pattern = if query.literal {
        escape_regex(&query.pattern)
    } else {
        query.pattern.clone()
    };
    let matcher = grep_regex::RegexMatcherBuilder::new()
        .case_insensitive(query.ignore_case)
        .build(&pattern)
        .map_err(|e| SearchError::InvalidPattern(e.to_string()))?;

    let mut walker = WalkBuilder::new(&search_path);
    walker
        .hidden(false)
        .git_ignore(true)
        .git_exclude(true)
        .filter_entry(|entry| entry.file_name() != ".git")
        .sort_by_file_path(Path::cmp);

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

    let mut searcher = SearcherBuilder::new()
        .line_number(true)
        .binary_detection(BinaryDetection::quit(b'\x00'))
        .build();

    // The two budget shapes unfold into three plain numbers so the loop below
    // has one code path.
    let (max_matches, max_files, per_file) = match query.limit {
        SearchLimit::Matches(n) => (n.max(1), usize::MAX, usize::MAX),
        SearchLimit::Files {
            max_files,
            per_file,
        } => (usize::MAX, max_files.max(1), per_file.max(1)),
    };

    let mut matches: Vec<SearchMatch> = Vec::new();
    // Shared counter lets the per-file sink stop the whole search at the
    // limit. Atomic because sink closures can't borrow `matches` mutably
    // while the outer loop also does.
    let count = Arc::new(AtomicUsize::new(0));
    let mut files_with_matches = 0_usize;
    let mut limit_reached = false;

    // Stored, single-threaded walk => deterministic result order.
    for entry in walker.build() {
        if cancel.is_cancelled() {
            return Err(SearchError::Aborted);
        }
        if count.load(Ordering::Relaxed) >= max_matches || files_with_matches >= max_files {
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
                // Returning Ok(false) stops the search in this file: at the
                // global match limit or at the per-file cap.
                Ok(seen < max_matches && file_matches.len() < per_file)
            }),
        );
        // Unreadable files are skipped silently, matching rg's behavior of
        // printing a warning and moving on.
        if result.is_err() {
            continue;
        }
        if !file_matches.is_empty() {
            files_with_matches += 1;
        }
        matches.extend(file_matches);
    }

    if count.load(Ordering::Relaxed) >= max_matches || files_with_matches >= max_files {
        limit_reached = true;
        matches.truncate(max_matches);
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
            limit: SearchLimit::Matches(100),
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
        query.limit = SearchLimit::Matches(2);
        let outcome = run(&root, query);
        assert_eq!(outcome.matches.len(), 2);
        assert!(outcome.limit_reached);
    }

    /// Paths relative to `root`, forward slashes, in result order.
    fn relative_paths(root: &Path, outcome: &SearchOutcome) -> Vec<String> {
        outcome
            .matches
            .iter()
            .map(|m| {
                m.path
                    .strip_prefix(root)
                    .unwrap()
                    .display()
                    .to_string()
                    .replace('\\', "/")
            })
            .collect()
    }

    #[test]
    fn walk_order_is_sorted_by_path() {
        let root = temp_root("sorted");
        // Written in reverse order on purpose: the walk must not depend on
        // creation or readdir order.
        write_tree(
            &root,
            &[
                ("zeta.txt", "hit\n"),
                ("beta/inner.txt", "hit\n"),
                ("alpha.txt", "hit\n"),
            ],
        );
        let outcome = run(&root, base_query("hit"));
        assert_eq!(
            relative_paths(&root, &outcome),
            ["alpha.txt", "beta/inner.txt", "zeta.txt"]
        );
    }

    #[test]
    fn files_limit_caps_per_file_and_stops_at_max_files() {
        let root = temp_root("fileslimit");
        write_tree(
            &root,
            &[
                ("a.txt", "x\nx\nx\nx\n"),
                ("b.txt", "x\n"),
                ("c.txt", "x\n"),
            ],
        );
        let mut query = base_query("x");
        query.limit = SearchLimit::Files {
            max_files: 2,
            per_file: 2,
        };
        let outcome = run(&root, query);
        // a.txt is capped at 2 of its 4 lines, b.txt takes the second file
        // slot, c.txt is never opened.
        assert_eq!(relative_paths(&root, &outcome), ["a.txt", "a.txt", "b.txt"]);
        assert!(outcome.limit_reached);
    }
}
