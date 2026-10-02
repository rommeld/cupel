//! Explicit project trust, stored outside the repository in cupel home.
//!
//! Decisions apply only to an exact canonical cwd, never to descendants.
//! Missing, unreadable, or malformed state is always restricted.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProjectTrust {
    Restricted,
    Trusted,
}

fn load(path: &Path) -> std::io::Result<BTreeMap<PathBuf, ProjectTrust>> {
    match std::fs::read_to_string(path) {
        Ok(content) => serde_json::from_str(&content).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("invalid project trust file {}: {e}", path.display()),
            )
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(e) => Err(e),
    }
}

/// `None` means no usable decision yet, not permission to trust the project.
#[must_use]
pub fn decision(home: Option<&Path>, cwd: &Path) -> Option<ProjectTrust> {
    let cwd = cwd.canonicalize().ok()?;
    let path = home?.join("project-trust.json");
    match load(&path) {
        Ok(decisions) => decisions.get(&cwd).copied(),
        Err(e) => {
            tracing::warn!(path = %path.display(), "project trust unavailable: {e}");
            None
        }
    }
}

#[must_use]
pub fn is_trusted(home: Option<&Path>, cwd: &Path) -> bool {
    decision(home, cwd) == Some(ProjectTrust::Trusted)
}

/// Save only after a user decision. Repository settings cannot grant trust.
/// Atomic replacement avoids partially written grants; malformed state is
/// refused rather than silently overwriting other projects' decisions.
pub fn save(home: &Path, cwd: &Path, trust: ProjectTrust) -> std::io::Result<()> {
    let cwd = cwd.canonicalize()?;
    let path = home.join("project-trust.json");
    let mut decisions = load(&path)?;
    decisions.insert(cwd, trust);
    let body = serde_json::to_vec_pretty(&decisions).map_err(std::io::Error::other)?;
    std::fs::create_dir_all(home)?;

    let tmp = home.join(format!("project-trust.json.{}.tmp", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(&tmp)?;
    let result = (|| {
        file.write_all(&body)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        std::fs::rename(&tmp, &path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use crate::project_trust::{ProjectTrust, decision, is_trusted, save};

    fn root(name: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("cupel-project-trust-{name}"));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("project/child")).unwrap();
        root
    }

    #[test]
    fn decisions_are_persistent_exact_and_home_only() {
        let root = root("decisions");
        let home = root.join("home");
        let cwd = root.join("project");
        assert_eq!(decision(Some(&home), &cwd), None);
        std::fs::create_dir_all(cwd.join(".cupel")).unwrap();
        std::fs::write(cwd.join(".cupel/project-trust.json"), r#"{"trusted":true}"#).unwrap();
        assert!(!is_trusted(Some(&home), &cwd));
        assert!(!is_trusted(None, &cwd));

        save(&home, &cwd, ProjectTrust::Trusted).unwrap();
        assert!(is_trusted(Some(&home), &cwd.join(".")));
        assert!(!is_trusted(Some(&home), &cwd.join("child")));
        assert!(!is_trusted(Some(&home), &root.join("missing")));

        save(&home, &cwd, ProjectTrust::Restricted).unwrap();
        assert_eq!(decision(Some(&home), &cwd), Some(ProjectTrust::Restricted));
        assert!(!is_trusted(Some(&home), &cwd));
    }

    #[test]
    fn malformed_state_fails_closed_and_is_not_overwritten() {
        let root = root("malformed");
        let path = root.join("project-trust.json");
        std::fs::write(&path, "not json").unwrap();
        assert!(!is_trusted(Some(&root), &root.join("project")));
        assert!(save(&root, &root.join("project"), ProjectTrust::Trusted).is_err());
        assert_eq!(std::fs::read_to_string(path).unwrap(), "not json");
    }

    #[cfg(unix)]
    #[test]
    fn symlink_alias_uses_the_same_canonical_decision() {
        let root = root("alias");
        let alias = root.join("alias");
        std::os::unix::fs::symlink(root.join("project"), &alias).unwrap();
        save(&root, &alias, ProjectTrust::Trusted).unwrap();
        assert!(is_trusted(Some(&root), &root.join("project")));
    }
}
