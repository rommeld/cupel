//! Project-context and skill loading.
//!
//! Two kinds of resources, two loading strategies - the difference matters:
//!
//! - **Context files** (`AGENTS.md` / `CLAUDE.md`) are loaded EAGERLY: their
//!   full contents ride in the system prompt of every request. They hold
//!   standing project instructions ("run clippy after edits", "tests live in
//!   tests/") that must always be visible.
//! - **Skills** (`SKILL.md` files) are loaded LAZILY: only a catalog line
//!   (name + description + path) enters the system prompt, plus the
//!   instruction to `read` the full file when a task matches. This is
//!   progressive disclosure - twenty skills cost twenty lines, not twenty
//!   documents, and the model pulls in exactly the one it needs.
//!
//! Both come from the same two source roots, searched in order:
//! 1. the binary installation directory (ships defaults alongside `cupel`),
//! 2. the project working directory (project-specific, wins by coming last
//!    in the prompt).
//!
//! Layout per root: `AGENTS.md`/`CLAUDE.md` directly in the root,
//! `skills/<skill-name>/SKILL.md` below it.

use std::path::{Path, PathBuf};

/// One eagerly-loaded context file.
#[derive(Debug, Clone)]
pub struct ContextFile {
    pub path: PathBuf,
    pub content: String,
}

/// One lazily-loaded skill: catalog data only; the model reads the file.
#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
}

/// The source roots to search. `None` entries (e.g. no resolvable exe dir)
/// are skipped, so callers can pass results straight in.
#[must_use]
pub fn default_roots(cwd: &Path) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    // Binary installation source: the directory the executable lives in.
    if let Ok(exe) = std::env::current_exe()
        && let Some(dir) = exe.parent()
        // Skip when running from a cargo target dir - target/debug/AGENTS.md
        // would be surprising. Installed binaries live elsewhere.
        && !dir.components().any(|c| c.as_os_str() == "target")
    {
        roots.push(dir.to_path_buf());
    }
    // Project source. Comes second so project instructions appear AFTER
    // (and therefore effectively override) installation-wide ones.
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

/// How deep below `<root>/skills` the scan looks for SKILL.md. The
/// convention is one directory per skill (depth 1); a little slack allows
/// grouping (e.g. `skills/rust/testing/SKILL.md`) without letting a stray
/// `node_modules` turn discovery into a filesystem crawl.
const SKILL_SCAN_DEPTH: usize = 3;

/// Discover skills under `<root>/skills/` for each root.
#[must_use]
pub fn discover_skills(roots: &[PathBuf]) -> Vec<Skill> {
    let mut skills = Vec::new();
    for root in roots {
        scan_for_skills(&root.join("skills"), SKILL_SCAN_DEPTH, &mut skills);
    }
    skills
}

fn scan_for_skills(dir: &Path, depth_left: usize, out: &mut Vec<Skill>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return; // No skills directory here - perfectly normal.
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if depth_left > 0 {
                scan_for_skills(&path, depth_left - 1, out);
            }
        } else if path.file_name().is_some_and(|n| n == "SKILL.md") {
            match parse_skill(&path) {
                Some(skill) => out.push(skill),
                None => {
                    tracing::debug!(path = %path.display(), "skipping skill without a description");
                }
            }
        }
    }
}

/// Parse one SKILL.md into catalog data. The name and description come from
/// YAML-ish frontmatter:
///
/// ```text
/// ---
/// name: commit-style
/// description: How to format commit messages in this repo
/// ---
/// ```
///
/// Lenient fallbacks: a missing name uses the parent directory's name; a
/// missing description falls back to the first non-heading text line. A
/// skill with no description at all is skipped - the description is what
/// the model matches tasks against, so without one the skill is dead
/// weight in the prompt.
fn parse_skill(path: &Path) -> Option<Skill> {
    let content = std::fs::read_to_string(path).ok()?;
    let (frontmatter, body) = split_frontmatter(&content);

    let name = frontmatter
        .iter()
        .find(|(key, _)| key == "name")
        .map(|(_, value)| value.clone())
        .or_else(|| {
            path.parent()?
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
        })?;

    let description = frontmatter
        .iter()
        .find(|(key, _)| key == "description")
        .map(|(_, value)| value.clone())
        .or_else(|| {
            body.lines()
                .map(str::trim)
                .find(|line| !line.is_empty() && !line.starts_with('#'))
                .map(str::to_string)
        })?;

    Some(Skill {
        name,
        description,
        path: path.to_path_buf(),
    })
}

/// Split `---`-fenced frontmatter into key/value pairs plus the body.
/// A hand-rolled ~20-line parser instead of a YAML dependency: skill
/// frontmatter is flat `key: value` lines by convention, nothing more.
fn split_frontmatter(content: &str) -> (Vec<(String, String)>, &str) {
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
    fn skills_are_discovered_with_frontmatter() {
        let root = temp_root("skills");
        let skill_dir = root.join("skills/commit-style");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "---\nname: commit-style\ndescription: How to write commits\n---\n\nLong instructions here.",
        )
        .unwrap();
        // Nested grouping still within scan depth.
        let nested = root.join("skills/rust/testing");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(
            nested.join("SKILL.md"),
            "---\ndescription: Rust testing conventions\n---\nBody.",
        )
        .unwrap();

        let mut skills = discover_skills(&[root]);
        skills.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(skills.len(), 2);
        assert_eq!(skills[0].name, "commit-style");
        assert_eq!(skills[0].description, "How to write commits");
        // Missing name falls back to the parent directory.
        assert_eq!(skills[1].name, "testing");
    }

    #[test]
    fn skill_without_any_description_is_skipped() {
        let root = temp_root("no-desc");
        let skill_dir = root.join("skills/broken");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), "# Only a heading\n").unwrap();
        assert!(discover_skills(&[root]).is_empty());
    }

    #[test]
    fn description_falls_back_to_first_body_line() {
        let root = temp_root("body-desc");
        let skill_dir = root.join("skills/plain");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(
            skill_dir.join("SKILL.md"),
            "# Title\n\nUse this when formatting SQL.\n",
        )
        .unwrap();
        let skills = discover_skills(&[root]);
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].description, "Use this when formatting SQL.");
        assert_eq!(skills[0].name, "plain");
    }
}
