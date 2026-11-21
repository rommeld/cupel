//! Discover skill metadata without loading instructions or running bundled scripts.
//! Set `disable: true` in `SKILL.md` frontmatter to omit the skill from the
//! session catalog. Missing `disable` and `disable: false` keep it enabled.

use std::collections::BTreeMap;
use std::fs;
use std::io::{self, BufRead as _, BufReader, Read as _};
use std::path::{self, Path, PathBuf};

use serde::Serialize;
use yaml_serde::Value;

// Bound startup reads even if a malformed file has no closing fence.
const MAX_FRONTMATTER_BYTES: u64 = 16 * 1024;

/// A catalog entry; `path` is absolute and can be passed to `read`.
#[derive(Debug, Clone, Serialize)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub path: String,
}

struct SkillEntry {
    skill: Skill,
    disabled: bool,
}

/// Scan the home and project skill directories in precedence order. Disabled
/// project entries mask home entries; malformed project entries do not.
pub(crate) fn discover(home: Option<&Path>, cwd: &Path) -> (Vec<Skill>, Vec<String>) {
    let project = cwd.join(".cupel");
    let mut roots = Vec::new();
    let mut warnings = Vec::new();
    for base in home.into_iter().chain([project.as_path()]) {
        let root = base.join("skills");
        match path::absolute(&root) {
            Ok(root) if !roots.contains(&root) => roots.push(root),
            Ok(_) => {}
            Err(error) => warnings.push(load_failure(&root, &error)),
        }
    }

    let mut catalog = BTreeMap::new();
    for root in roots {
        catalog.extend(load_root(&root, &mut warnings));
    }
    let enabled = catalog
        .into_values()
        .filter_map(|entry| (!entry.disabled).then_some(entry.skill))
        .collect();
    (enabled, warnings)
}

fn load_root(root: &Path, warnings: &mut Vec<String>) -> BTreeMap<String, SkillEntry> {
    let mut skills = BTreeMap::new();
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return skills,
        Err(error) => {
            warnings.push(load_failure(root, &error));
            return skills;
        }
    };

    let mut paths: Vec<PathBuf> = Vec::new();
    for entry in entries {
        match entry {
            Ok(entry) => paths.push(entry.path().join("SKILL.md")),
            Err(error) => warnings.push(load_failure(root, &error)),
        }
    }
    paths.sort_unstable();

    for path in paths {
        match fs::metadata(&path) {
            Ok(metadata) if metadata.is_file() => {}
            Ok(_) => {
                warnings.push(load_failure(&path, &invalid_data("not a regular file")));
                continue;
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) =>
            {
                continue;
            }
            Err(error) => {
                warnings.push(load_failure(&path, &error));
                continue;
            }
        }

        match load_skill(&path) {
            Ok(entry) if !skills.contains_key(&entry.skill.name) => {
                skills.insert(entry.skill.name.clone(), entry);
            }
            Ok(entry) => warnings.push(load_failure(
                &path,
                &invalid_data(format!("duplicate skill name {:?}", entry.skill.name)),
            )),
            Err(error) => warnings.push(load_failure(&path, &error)),
        }
    }
    skills
}

fn load_skill(path: &Path) -> io::Result<SkillEntry> {
    let path_text = path
        .to_str()
        .ok_or_else(|| invalid_data("path is not valid UTF-8"))?;
    let file = fs::File::open(path)?;
    let mut reader = BufReader::new(file.take(MAX_FRONTMATTER_BYTES + 1));
    let mut line = String::new();
    let mut bytes_read = reader.read_line(&mut line)? as u64;
    if line.trim_end_matches(['\r', '\n']) != "---" {
        return Err(invalid_data("expected opening YAML frontmatter fence"));
    }

    let mut yaml = String::new();
    loop {
        line.clear();
        let len = reader.read_line(&mut line)?;
        bytes_read += len as u64;
        if bytes_read > MAX_FRONTMATTER_BYTES {
            return Err(invalid_data("YAML frontmatter exceeds 16 KiB"));
        }
        if len == 0 {
            return Err(invalid_data("missing closing YAML frontmatter fence"));
        }
        if line.trim_end_matches(['\r', '\n']) == "---" {
            break;
        }
        yaml.push_str(&line);
    }

    let metadata: Value = yaml_serde::from_str(&yaml).map_err(invalid_data)?;
    let name = required_string(&metadata, "name")?;
    if name.chars().any(char::is_control) {
        return Err(invalid_data("skill name contains a control character"));
    }
    let disabled = match metadata.get("disable") {
        None => false,
        Some(value) => value
            .as_bool()
            .ok_or_else(|| invalid_data("expected a boolean for disable"))?,
    };
    let skill = Skill {
        name: name.to_string(),
        description: required_string(&metadata, "description")?
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" "),
        path: path_text.to_string(),
    };
    Ok(SkillEntry { skill, disabled })
}

fn required_string<'a>(metadata: &'a Value, key: &str) -> io::Result<&'a str> {
    metadata
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .ok_or_else(|| invalid_data(format!("expected a nonempty string for {key}")))
}

fn invalid_data(error: impl Into<Box<dyn std::error::Error + Send + Sync>>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, error)
}

fn load_failure(path: &Path, error: &io::Error) -> String {
    format!(
        "It was not possible to load the skill. {}: {error}",
        path.display()
    )
}
