//! Session snapshots of project instructions, the skill catalog, and linked guides.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{self, Path, PathBuf};
use std::sync::Arc;

use pulldown_cmark::{Event, Options, Parser, Tag};

use crate::read_filesystem::{LocalFiles, ReadFilesystem, SnapshotKind};
use crate::resources::{self, ContextFile};
use crate::skills::Skill;

// Automatically loaded guides are bounded independently of read's output pagination.
const MAX_GUIDE_BYTES: u64 = 1024 * 1024;

pub(crate) struct InstructionCache {
    roots: Vec<PathBuf>,
    aliases: HashMap<PathBuf, PathBuf>,
    files: HashMap<PathBuf, Arc<str>>,
    skills: HashMap<PathBuf, Skill>,
}

pub(crate) struct InstructionRead {
    pub content: Arc<str>,
    pub skill_name: Option<String>,
    pub warnings: Vec<String>,
}

impl InstructionCache {
    pub async fn load(
        roots: &[PathBuf],
        skills: &[Skill],
    ) -> (Self, Vec<ContextFile>, Vec<String>) {
        let mut cache = Self {
            roots: roots
                .iter()
                .filter_map(|root| fs::canonicalize(root).ok())
                .collect(),
            aliases: HashMap::new(),
            files: HashMap::new(),
            skills: HashMap::new(),
        };
        for skill in skills {
            let alias = PathBuf::from(&skill.path);
            if let Ok(path) = fs::canonicalize(&alias) {
                cache.aliases.insert(alias, path.clone());
                cache.skills.insert(path, skill.clone());
            }
        }
        let context = resources::load_context_files_with(roots, |path| cache.load_context(path));
        let mut warnings = Vec::new();
        for file in &context {
            warnings.extend(
                cache
                    .load_guides(&file.path, &file.content, &mut LocalFiles)
                    .await,
            );
        }
        (cache, context, warnings)
    }

    fn load_context(&mut self, alias: &Path) -> io::Result<String> {
        let alias = path::absolute(alias)?;
        let path = fs::canonicalize(&alias)?;
        let content = match self.files.get(&path) {
            Some(content) => content.to_string(),
            None => {
                let content = fs::read_to_string(&path)?;
                self.files.insert(path.clone(), Arc::from(content.as_str()));
                content
            }
        };
        self.aliases.insert(alias, path);
        Ok(content)
    }

    /// Preserve the identity of a known path even if it is removed or retargeted.
    pub fn source_path(&self, requested: &Path) -> Option<PathBuf> {
        let alias = path::absolute(requested).ok()?;
        if self.files.contains_key(&alias) || self.skills.contains_key(&alias) {
            return Some(alias);
        }
        self.aliases.get(&alias).cloned()
    }

    /// Caller holds the session cache lock so concurrent skill reads share the first load.
    pub async fn read(
        &mut self,
        path: &Path,
        files: &mut impl ReadFilesystem,
    ) -> io::Result<Option<InstructionRead>> {
        let skill = self.skills.get(path);
        if let Some(content) = self.files.get(path) {
            return Ok(Some(InstructionRead {
                content: Arc::clone(content),
                skill_name: skill.map(|skill| skill.name.clone()),
                warnings: Vec::new(),
            }));
        }
        let Some(skill) = skill else { return Ok(None) };
        let name = skill.name.clone();
        let declared_path = PathBuf::from(&skill.path);
        let content: Arc<str> = files.snapshot(path, SnapshotKind::Skill).await?.into();
        let warnings = self.load_guides(&declared_path, &content, files).await;
        self.files.insert(path.to_path_buf(), Arc::clone(&content));
        Ok(Some(InstructionRead {
            content,
            skill_name: Some(name),
            warnings,
        }))
    }

    async fn load_guides(
        &mut self,
        declaring_path: &Path,
        content: &str,
        files: &mut impl ReadFilesystem,
    ) -> Vec<String> {
        let paths: Vec<_> = Parser::new_ext(content, Options::ENABLE_YAML_STYLE_METADATA_BLOCKS)
            .filter_map(|event| match event {
                Event::Start(Tag::Link { dest_url, .. }) => guide_path(declaring_path, &dest_url),
                _ => None,
            })
            .collect();
        let mut warnings = Vec::new();
        for path in paths {
            if let Err(error) = self.load_guide(&path, files).await {
                warnings.push(format!(
                    "Could not cache instruction guide {}: {error}",
                    path.display()
                ));
            }
        }
        warnings
    }

    async fn load_guide(&mut self, path: &Path, files: &mut impl ReadFilesystem) -> io::Result<()> {
        if self.source_path(path).is_some() {
            return Ok(());
        }
        let alias = path::absolute(path)?;
        let path = files.canonicalize(&alias).await?;
        if !self.roots.iter().any(|root| path.starts_with(root)) {
            return Err(io::Error::other("outside the project and Cupel home"));
        }
        // A link to a skill is not a guide and must not activate it.
        if self.skills.contains_key(&path) || is_skill_path(&path) {
            return Ok(());
        }
        if !self.files.contains_key(&path) {
            let content = files
                .snapshot(&path, SnapshotKind::Guide(MAX_GUIDE_BYTES))
                .await?;
            self.files.insert(path.clone(), content.into());
        }
        self.aliases.insert(alias, path);
        Ok(())
    }
}

fn guide_path(declaring_path: &Path, destination: &str) -> Option<PathBuf> {
    let destination = destination.split(['#', '?']).next()?;
    let decoded = percent_encoding::percent_decode_str(destination)
        .decode_utf8()
        .ok()?;
    let path = Path::new(decoded.as_ref());
    if path.is_absolute() || decoded.contains([':', '\\']) || is_skill_path(path) {
        return None;
    }
    let extension = path.extension()?.to_str()?;
    let parent = declaring_path.parent()?;
    (extension.eq_ignore_ascii_case("md") || extension.eq_ignore_ascii_case("markdown"))
        .then(|| parent.join(path))
}

fn is_skill_path(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.eq_ignore_ascii_case("SKILL.md"))
}
