use std::io;
use std::path::{Path, PathBuf};

use tokio::io::AsyncReadExt as _;

use crate::source_observations;
use crate::tools::read::{ReadArgs, TextPage};

#[derive(Clone, Copy)]
#[cfg_attr(
    feature = "codemode-preview",
    derive(serde::Serialize, serde::Deserialize)
)]
pub(crate) enum SnapshotKind {
    Skill,
    Guide(u64),
}

#[cfg_attr(
    feature = "codemode-preview",
    derive(serde::Serialize, serde::Deserialize)
)]
pub(crate) struct PageRevision {
    pub page: TextPage,
    pub revision: String,
}

#[async_trait::async_trait]
pub(crate) trait ReadFilesystem: Send {
    async fn canonicalize(&mut self, path: &Path) -> io::Result<PathBuf>;
    async fn snapshot(&mut self, path: &Path, kind: SnapshotKind) -> io::Result<String>;
    async fn page(&mut self, path: &Path, args: &ReadArgs) -> io::Result<PageRevision>;
}

pub(crate) struct LocalFiles;

#[async_trait::async_trait]
impl ReadFilesystem for LocalFiles {
    async fn canonicalize(&mut self, path: &Path) -> io::Result<PathBuf> {
        tokio::fs::canonicalize(path).await
    }

    async fn snapshot(&mut self, path: &Path, kind: SnapshotKind) -> io::Result<String> {
        match kind {
            SnapshotKind::Skill => tokio::fs::read_to_string(path).await,
            SnapshotKind::Guide(limit) => {
                if !tokio::fs::metadata(path).await?.is_file() {
                    return Err(io::Error::other("not a regular file"));
                }
                let mut content = String::new();
                tokio::fs::File::open(path)
                    .await?
                    .take(limit.saturating_add(1))
                    .read_to_string(&mut content)
                    .await?;
                if content.len() as u64 > limit {
                    return Err(io::Error::other("exceeds the 1 MiB guide limit"));
                }
                Ok(content)
            }
        }
    }

    async fn page(&mut self, path: &Path, args: &ReadArgs) -> io::Result<PageRevision> {
        let content = tokio::fs::read_to_string(path).await.map_err(|error| {
            io::Error::other(format!("Could not read file: {} ({error})", args.path))
        })?;
        let page = TextPage::from_source(&content, args).map_err(io::Error::other)?;
        Ok(PageRevision {
            page,
            revision: source_observations::revision(content.as_bytes()),
        })
    }
}
