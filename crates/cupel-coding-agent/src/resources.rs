//! Project-context loading.
//!
//! Context files (`AGENTS.md` / `CLAUDE.md`) are loaded EAGERLY: their full
//! contents ride in the system prompt of every request. They hold standing
//! project instructions ("run clippy after edits", "tests live in tests/")
//! that must always be visible.
//!
//! They come from two source roots, searched in order:
//! 1. the cupel home (`~/.cupel`, override with `CUPEL_HOME`) - the cargo
//!    layout: the same directory also holds `bin/cupel` (the installed
//!    binary), `prompts/` (global `/command` templates), and the reserved
//!    `memory/` for the future memory feature,
//! 2. the project working directory (project-specific, wins by coming last
//!    in the prompt).

use std::path::{Path, PathBuf};

/// One eagerly-loaded context file.
#[derive(Debug, Clone)]
pub struct ContextFile {
    pub path: PathBuf,
    pub content: String,
}

/// The cupel home: `CUPEL_HOME` when set, else `~/.cupel`. `None` only when
/// neither the env var nor a home directory can be resolved.
#[must_use]
pub fn config_home() -> Option<PathBuf> {
    resolve_config_home(std::env::var("CUPEL_HOME").ok(), std::env::home_dir())
}

/// The pure core of [`config_home`], parameterized so tests never have to
/// mutate process-global environment variables (racy across parallel tests).
fn resolve_config_home(env_value: Option<String>, home: Option<PathBuf>) -> Option<PathBuf> {
    match env_value {
        Some(value) if !value.trim().is_empty() => Some(PathBuf::from(value)),
        _ => Some(home?.join(".cupel")),
    }
}

/// The source roots to search: cupel home first, project cwd second (later
/// roots' instructions appear AFTER earlier ones in the prompt, so project
/// context effectively overrides global context).
#[must_use]
pub fn default_roots(cwd: &Path) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Some(home) = config_home() {
        roots.push(home);
    }
    roots.push(cwd.to_path_buf());
    roots
}

/// Candidate context-file names, in preference order. Only the FIRST match
/// per root is loaded: `CLAUDE.md` is conventionally a copy of `AGENTS.md`,
/// and loading both would duplicate every instruction. (Same rule as pi.)
const CONTEXT_FILE_CANDIDATES: &[&str] = &["AGENTS.md", "AGENTS.MD", "CLAUDE.md", "CLAUDE.MD"];

/// Eagerly load the context file (if any) from each root.
#[must_use]
pub fn load_context_files(roots: &[PathBuf]) -> Vec<ContextFile> {
    let mut files = Vec::new();
    for root in roots {
        for candidate in CONTEXT_FILE_CANDIDATES {
            let path = root.join(candidate);
            if let Ok(content) = std::fs::read_to_string(&path) {
                if !content.trim().is_empty() {
                    tracing::debug!(path = %path.display(), bytes = content.len(), "context file loaded");
                    files.push(ContextFile { path, content });
                }
                break; // First candidate wins for this root.
            }
        }
    }
    files
}

/// Split `---`-fenced frontmatter into key/value pairs plus the body.
/// A hand-rolled ~20-line parser instead of a YAML dependency: prompt
/// template frontmatter is flat `key: value` lines by convention.
pub(crate) fn split_frontmatter(content: &str) -> (Vec<(String, String)>, &str) {
    let Some(rest) = content.strip_prefix("---") else {
        return (Vec::new(), content);
    };
    let Some(end) = rest.find("\n---") else {
        return (Vec::new(), content);
    };
    let frontmatter = &rest[..end];
    let body = rest[end + 4..].trim_start_matches(['\r', '\n']);

    let pairs = frontmatter
        .lines()
        .filter_map(|line| {
            let (key, value) = line.split_once(':')?;
            let value = value.trim();
            (!value.is_empty()).then(|| (key.trim().to_string(), value.to_string()))
        })
        .collect();
    (pairs, body)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cupel-resources-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn agents_md_wins_over_claude_md() {
        let root = temp_root("preference");
        std::fs::write(root.join("AGENTS.md"), "agents instructions").unwrap();
        std::fs::write(root.join("CLAUDE.md"), "claude instructions").unwrap();
        let files = load_context_files(&[root]);
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].content, "agents instructions");
    }

    #[test]
    fn claude_md_is_the_fallback() {
        let root = temp_root("fallback");
        std::fs::write(root.join("CLAUDE.md"), "claude instructions").unwrap();
        let files = load_context_files(&[root]);
        assert_eq!(files.len(), 1);
        assert!(files[0].path.ends_with("CLAUDE.md"));
    }

    #[test]
    fn missing_and_empty_context_files_are_skipped() {
        let root = temp_root("empty");
        std::fs::write(root.join("AGENTS.md"), "   \n").unwrap();
        assert!(load_context_files(&[root]).is_empty());
        assert!(load_context_files(&[PathBuf::from("/nonexistent-cupel")]).is_empty());
    }

    #[test]
    fn config_home_env_override_wins() {
        assert_eq!(
            resolve_config_home(Some("/custom/home".into()), Some(PathBuf::from("/u"))),
            Some(PathBuf::from("/custom/home"))
        );
        // Blank override falls through to the default.
        assert_eq!(
            resolve_config_home(Some("  ".into()), Some(PathBuf::from("/u"))),
            Some(PathBuf::from("/u/.cupel"))
        );
    }

    #[test]
    fn config_home_defaults_to_dot_cupel_or_none() {
        assert_eq!(
            resolve_config_home(None, Some(PathBuf::from("/u"))),
            Some(PathBuf::from("/u/.cupel"))
        );
        assert_eq!(resolve_config_home(None, None), None);
    }

    #[test]
    fn default_roots_end_with_the_project_cwd() {
        let roots = default_roots(Path::new("/proj"));
        assert_eq!(roots.last(), Some(&PathBuf::from("/proj")));
        // First root is the cupel home whenever one resolves.
        assert!(roots.len() <= 2);
    }
}
