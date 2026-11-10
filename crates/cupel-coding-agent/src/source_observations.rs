//! Bounded, live source observations shared by read and patch within one session.
//!
//! Revisions identify the exact bytes read, not comprehension, filesystem
//! isolation, or verification outcomes. Nothing here is persisted as memory.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use sha2::{Digest as _, Sha256};
use tokio::sync::Mutex;

use crate::search::source_locator;

const MAX_OBSERVATIONS: usize = 256;

/// Inclusive, one-based source lines actually delivered to the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceRange {
    pub start: usize,
    pub end: usize,
}

/// An immutable observation; its id never refers to different source bytes or a
/// different view. Paths identify the checkout and file, not just a basename.
#[derive(Debug, Clone)]
pub struct SourceObservation {
    pub id: String,
    pub checkout: PathBuf,
    pub path: PathBuf,
    pub revision: String,
    pub range: Option<SourceRange>,
    pub total_lines: usize,
}

impl SourceObservation {
    /// Compact provenance that travels in tool content, not only UI metadata.
    #[must_use]
    pub fn header(&self) -> String {
        let range = self.range.map_or_else(
            || "none".to_string(),
            |range| {
                format!(
                    "{}-{} of {} lines",
                    range.start, range.end, self.total_lines
                )
            },
        );
        let coverage = match self.range {
            Some(SourceRange { start: 1, end }) if end == self.total_lines => "complete file",
            Some(_) => "partial file",
            None => "no source returned",
        };
        format!(
            "[Observation: {}\nCheckout: {}\nPath: {}\nRevision: {}\nRange: {range}\nCoverage: {coverage}]",
            self.id,
            self.checkout.display(),
            source_locator(&self.path, &self.checkout),
            self.revision,
        )
    }

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

    /// Record provenance from the bytes that actually supplied a view. Callers
    /// resolve checkout/file identities before reading, never by a later rehash.
    pub async fn record(
        &self,
        checkout: &Path,
        path: &Path,
        bytes: &[u8],
        range: Option<SourceRange>,
        total_lines: usize,
    ) -> Result<SourceObservation, String> {
        let revision = revision(bytes);
        let mut state = self.state.lock().await;
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
            range,
            total_lines,
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
    format!("sha256:{}", hex(&Sha256::digest(bytes)))
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
        let first = store
            .record(
                checkout,
                path,
                b"old",
                Some(SourceRange { start: 1, end: 1 }),
                1,
            )
            .await
            .unwrap();
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
        for _ in 0..MAX_OBSERVATIONS {
            let next = store.record(checkout, path, b"new", None, 1).await.unwrap();
            assert_ne!(first.id, next.id);
        }
        assert!(store.get(&first.id).await.unwrap_err().contains("expired"));
        let other = SourceObservations::new();
        let next = other.record(checkout, path, b"old", None, 1).await.unwrap();
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
