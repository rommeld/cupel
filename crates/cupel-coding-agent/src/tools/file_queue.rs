//! Per-file mutation serialization.
//!
//! The agent loop runs tool calls in PARALLEL by default. Two edits to the
//! same file racing each other means the second one reads stale content and
//! either fails to match or silently reverts the first - so mutations to the
//! same file must run one at a time, while different files stay parallel.
//!
//! pi builds this from chained promises. In Rust the same guarantee is one
//! `tokio::Mutex` per file: lock, do the work, drop the guard. Files are
//! keyed by their *canonical* path so `./src/a.rs` and `src/a.rs` (or a
//! symlink) share a lock; a file that doesn't exist yet (write tool) falls
//! back to the absolute path.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};

use tokio::sync::Mutex as AsyncMutex;

/// Global registry: canonical path -> its mutation lock.
///
/// A `std::sync::Mutex` protects the MAP (held only for microseconds while
/// looking up/inserting); the `tokio::sync::Mutex` inside is the actual
/// per-file lock (held across await points for the whole mutation). Mixing
/// the two like this is the standard pattern - never hold a std mutex across
/// an await.
fn registry() -> &'static StdMutex<HashMap<PathBuf, Arc<AsyncMutex<()>>>> {
    static REGISTRY: OnceLock<StdMutex<HashMap<PathBuf, Arc<AsyncMutex<()>>>>> = OnceLock::new();
    REGISTRY.get_or_init(|| StdMutex::new(HashMap::new()))
}

fn queue_key(path: &Path) -> PathBuf {
    // canonicalize resolves symlinks AND relative segments, but requires the
    // path to exist; fall back to the plain absolute path for new files.
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// Acquire the mutation lock for `path`, returning a guard. Mutating tools
/// hold the guard for their whole read-modify-write cycle:
///
/// ```ignore
/// let _guard = lock_file_for_mutation(&absolute_path).await;
/// // read, edit, write - no other mutation can interleave on this file
/// ```
pub async fn lock_file_for_mutation(path: &Path) -> tokio::sync::OwnedMutexGuard<()> {
    let lock = {
        let mut map = registry().lock().expect("file queue registry poisoned");
        Arc::clone(map.entry(queue_key(path)).or_default())
    };
    // Entries are never removed: a lock is 16 bytes and the set of files an
    // agent session touches is small. pi cleans up eagerly because its map
    // holds promise chains; ours holds nothing once unlocked.
    lock.lock_owned().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn same_file_mutations_are_serialized() {
        let path = PathBuf::from("/tmp/cupel-test-file-queue-same");
        let in_critical_section = Arc::new(AtomicUsize::new(0));
        let max_seen = Arc::new(AtomicUsize::new(0));

        let mut tasks = Vec::new();
        for _ in 0..8 {
            let path = path.clone();
            let in_critical_section = Arc::clone(&in_critical_section);
            let max_seen = Arc::clone(&max_seen);
            tasks.push(tokio::spawn(async move {
                let _guard = lock_file_for_mutation(&path).await;
                let now = in_critical_section.fetch_add(1, Ordering::SeqCst) + 1;
                max_seen.fetch_max(now, Ordering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_millis(2)).await;
                in_critical_section.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        for task in tasks {
            task.await.expect("task completes");
        }
        // Never more than one task inside the critical section at once.
        assert_eq!(max_seen.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn different_files_do_not_block_each_other() {
        let _guard_a = lock_file_for_mutation(Path::new("/tmp/cupel-test-queue-a")).await;
        // If files shared a lock this second acquisition would deadlock;
        // the timeout proves it doesn't.
        let guard_b = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            lock_file_for_mutation(Path::new("/tmp/cupel-test-queue-b")),
        )
        .await;
        assert!(guard_b.is_ok(), "distinct files must not share a lock");
    }
}
