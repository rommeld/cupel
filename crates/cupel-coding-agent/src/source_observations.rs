//! Bounded, live source observations shared by read and patch within one session.
//!
//! Revisions identify the exact bytes read or written, not comprehension, filesystem
//! isolation, or verification outcomes. Nothing here is persisted as memory.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use sha2::{Digest as _, Sha256};
use tokio::sync::Mutex;

const MAX_OBSERVATIONS: usize = 256;

/// An immutable observation of a file revision, independent of any read range.
#[derive(Debug, Clone)]
pub struct SourceObservation {
    pub id: String,
    pub checkout: PathBuf,
    pub path: PathBuf,
    pub revision: String,
}

impl SourceObservation {
    /// Whole-file precondition, checked against the same raw bytes used to
    /// construct the patch's normalized matching input.
    pub(crate) fn check(&self, path: &Path, bytes: &[u8]) -> Result<(), String> {
        if path != self.path || revision(bytes) != self.revision {
            return Err(format!(
                "Stale source observation {} for {}: file identity or bytes changed. Read the current file before retrying. Whole-file checks also reject unrelated edits.",
                self.id,
                self.path.display()
            ));
        }
        Ok(())
    }
}

#[derive(Default)]
struct State {
    namespace: Option<String>,
    next_id: u64,
    observations: VecDeque<SourceObservation>,
}

/// Session-owned, FIFO-bounded observation references. A new runtime, including
/// transcript resume, gets a new namespace; hot reload shares the existing one.
#[derive(Default)]
pub struct SourceObservations {
    state: Mutex<State>,
}

impl SourceObservations {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record the exact bytes read or written, reusing an ID for a retained revision.
    pub async fn record(
        &self,
        checkout: &Path,
        path: &Path,
        bytes: &[u8],
    ) -> Result<SourceObservation, String> {
        self.record_revision(checkout, path, revision(bytes)).await
    }

    pub(crate) async fn record_revision(
        &self,
        checkout: &Path,
        path: &Path,
        revision: String,
    ) -> Result<SourceObservation, String> {
        let mut state = self.state.lock().await;
        if let Some(observation) = state.observations.iter().find(|observation| {
            observation.checkout == checkout
                && observation.path == path
                && observation.revision == revision
        }) {
            return Ok(observation.clone());
        }
        if state.namespace.is_none() {
            let mut bytes = [0_u8; 16];
            getrandom::fill(&mut bytes)
                .map_err(|error| format!("Cannot create source observation namespace: {error}"))?;
            state.namespace = Some(hex(&bytes));
        }
        state.next_id = state
            .next_id
            .checked_add(1)
            .ok_or("Source observation ids exhausted")?;
        let namespace = state
            .namespace
            .as_deref()
            .ok_or("Source observation namespace unavailable")?;
        let observation = SourceObservation {
            id: format!("R{namespace}-{}", state.next_id),
            checkout: checkout.to_path_buf(),
            path: path.to_path_buf(),
            revision,
        };
        if state.observations.len() >= MAX_OBSERVATIONS {
            state.observations.pop_front();
        }
        state.observations.push_back(observation.clone());
        Ok(observation)
    }

    /// Missing, expired, and previous-runtime ids require renewed inspection;
    /// lookup never reinterprets them as the current file or another observation.
    pub async fn get(&self, id: &str) -> Result<SourceObservation, String> {
        let state = self.state.lock().await;
        state.observations.iter().find(|observation| observation.id == id)
            .cloned()
            .ok_or_else(|| format!(
                "Source observation {id} is unavailable or expired. Read the file again before retrying."
            ))
    }
}

fn revision(bytes: &[u8]) -> String {
    format_revision(&Sha256::digest(bytes))
}

pub(crate) fn format_revision(digest: &[u8]) -> String {
    format!("sha256:{}", hex(digest))
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;

    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(output, "{byte:02x}");
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn observations_are_bounded_immutable_and_not_reused_across_runtimes() {
        let store = SourceObservations::new();
        let checkout = Path::new("/checkout");
        let path = Path::new("/checkout/file");
        let first = store.record(checkout, path, b"old").await.unwrap();
        assert_eq!(
            store.get(&first.id).await.unwrap().revision,
            revision(b"old")
        );
        assert!(
            first
                .check(path, b"new")
                .unwrap_err()
                .contains("Stale source")
        );
        assert!(first.check(Path::new("/other/file"), b"old").is_err());
        for version in 0..MAX_OBSERVATIONS {
            let bytes = version.to_string();
            let next = store
                .record(checkout, path, bytes.as_bytes())
                .await
                .unwrap();
            assert_ne!(first.id, next.id);
        }
        assert!(store.get(&first.id).await.unwrap_err().contains("expired"));
        let other = SourceObservations::new();
        let next = other.record(checkout, path, b"old").await.unwrap();
        assert_ne!(first.id, next.id);
        assert!(other.get(&first.id).await.is_err());
    }

    #[test]
    fn revision_is_of_raw_bytes_not_normalized_text() {
        assert_ne!(revision(b"one\r\n"), revision(b"one\n"));
        assert_ne!(revision("\u{feff}one\n".as_bytes()), revision(b"one\n"));
        assert_eq!(
            revision(b"abc"),
            "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
