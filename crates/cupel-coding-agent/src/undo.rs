//! Session-local original-byte snapshots for conservative, per-user-turn undo.

use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use tokio::sync::{Mutex, MutexGuard};

use crate::session::SessionRecorder;
use crate::tools::file_queue::lock_file_for_mutation;

const UNAVAILABLE: &str = "Undo unavailable for this turn: files changed outside apply_patch.";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UndoCommand {
    Changes,
    Undo,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum Fingerprint {
    Missing,
    File { sha256: String, mode: Option<u32> },
}

impl Fingerprint {
    fn matches(&self, actual: &Self) -> bool {
        match (self, actual) {
            (Self::Missing, Self::Missing) => true,
            (
                Self::File { sha256, mode },
                Self::File {
                    sha256: actual_hash,
                    mode: actual_mode,
                },
            ) => sha256 == actual_hash && (mode.is_none() || mode == actual_mode),
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Entry {
    path: PathBuf,
    location: PathBuf,
    before: Fingerprint,
    after: Fingerprint,
    // Written before applying a patch. On a crash or partial failure, only the
    // previous or intended state is eligible for recovery, never arbitrary bytes.
    pending: Option<Fingerprint>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
enum TurnStatus {
    Recording,
    Undoing,
    Undone,
    Unavailable,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Manifest {
    version: u32,
    cwd: PathBuf,
    status: TurnStatus,
    entries: Vec<Entry>,
}

struct StoredTurn {
    directory: PathBuf,
    manifest: Manifest,
}

impl StoredTurn {
    fn save(&self) -> Result<(), String> {
        let bytes = serde_json::to_vec_pretty(&self.manifest).map_err(|e| e.to_string())?;
        atomic_write(&self.directory.join("manifest.json"), &bytes, None, None)
    }

    fn backup(&self, index: usize) -> PathBuf {
        self.directory.join("before").join(format!("{index:04}"))
    }

    fn original(&self, index: usize) -> Result<Option<Vec<u8>>, String> {
        let entry = &self.manifest.entries[index];
        match &entry.before {
            Fingerprint::Missing => Ok(None),
            Fingerprint::File { sha256, .. } => {
                let path = self.backup(index);
                let bytes = std::fs::read(&path)
                    .map_err(|e| format!("cannot read undo snapshot {}: {e}", path.display()))?;
                if hash(&bytes) != *sha256 {
                    return Err(format!(
                        "damaged undo snapshot for {}",
                        entry.path.display()
                    ));
                }
                Ok(Some(bytes))
            }
        }
    }
}

#[derive(Default)]
struct JournalState {
    active: Option<StoredTurn>,
}

/// One journal per recorder, shared with its patch tool and agent hooks.
/// Construction and turn boundaries do not create files; the first patch does.
pub struct UndoJournal {
    directory: Option<PathBuf>,
    cwd: PathBuf,
    state: Mutex<JournalState>,
}

impl UndoJournal {
    #[must_use]
    pub fn new(transcript: Option<&Path>, cwd: &Path) -> Self {
        Self {
            directory: transcript.map(|path| path.with_extension("").join("undo")),
            cwd: cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf()),
            state: Mutex::new(JournalState::default()),
        }
    }

    pub async fn begin_turn(&self) {
        self.state.lock().await.active = None;
    }

    /// Remove this session's snapshots after its tools and hooks have settled.
    /// Transcripts, helper files, and other sessions' journals are never removed.
    pub async fn cleanup(&self) -> Result<(), String> {
        let mut state = self.state.lock().await;
        let Some(root) = &self.directory else {
            return Ok(());
        };
        match tokio::fs::remove_dir_all(root).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "cannot remove undo snapshots {}: {error}",
                    root.display()
                ));
            }
        }
        state.active = None;
        // The session namespace may also contain helper transcripts. Only remove
        // the now-empty directory; never recursively delete its other contents.
        if let Some(parent) = root.parent() {
            match tokio::fs::remove_dir(parent).await {
                Ok(()) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
                    ) => {}
                Err(error) => {
                    return Err(format!(
                        "cannot remove empty undo namespace {}: {error}",
                        parent.display()
                    ));
                }
            }
        }
        Ok(())
    }

    pub(crate) fn enabled(&self) -> bool {
        self.directory.is_some()
    }

    pub(crate) async fn lock(&self) -> Recording<'_> {
        Recording {
            journal: self,
            state: self.state.lock().await,
        }
    }

    fn new_turn(&self) -> Result<StoredTurn, String> {
        let root = self
            .directory
            .as_deref()
            .ok_or("undo requires a cupel home")?;
        if let Some(parent) = root.parent() {
            private_directory(parent)?;
        }
        private_directory(root)?;
        let next = turn_directories(root)?
            .first()
            .map_or(0, |(number, _)| *number)
            .checked_add(1)
            .ok_or("undo turn counter exhausted")?;
        let directory = root.join(format!("{next:020}"));
        // A new session turn must never reuse another turn's snapshots.
        create_private_directory(&directory, false)?;
        private_directory(&directory.join("before"))?;
        let turn = StoredTurn {
            directory,
            manifest: Manifest {
                version: 1,
                cwd: self.cwd.clone(),
                status: TurnStatus::Recording,
                entries: Vec::new(),
            },
        };
        turn.save()?;
        Ok(turn)
    }

    fn latest(&self) -> Result<Option<StoredTurn>, String> {
        let root = self
            .directory
            .as_deref()
            .ok_or("undo requires a cupel home")?;
        for (_, directory) in turn_directories(root)? {
            let path = directory.join("manifest.json");
            let bytes = std::fs::read(&path)
                .map_err(|e| format!("cannot read undo manifest {}: {e}", path.display()))?;
            let manifest: Manifest = serde_json::from_slice(&bytes)
                .map_err(|e| format!("invalid undo manifest {}: {e}", path.display()))?;
            if manifest.version != 1 || manifest.cwd != self.cwd {
                return Err("undo journal version or working directory does not match".to_string());
            }
            if manifest.status == TurnStatus::Unavailable
                || (manifest.status != TurnStatus::Undone
                    && manifest
                        .entries
                        .iter()
                        .any(|entry| entry.before != entry.after || entry.pending.is_some()))
            {
                return Ok(Some(StoredTurn {
                    directory,
                    manifest,
                }));
            }
        }
        Ok(None)
    }

    /// Preview only the most recent edited turn, never unrelated working-tree edits.
    pub async fn changes(&self) -> Result<String, String> {
        let _state = self.state.lock().await;
        let Some(mut turn) = self.latest()? else {
            return Ok("No recorded patch edits to undo.".to_string());
        };
        if turn.manifest.status == TurnStatus::Unavailable {
            return Err(UNAVAILABLE.to_string());
        }
        let _guards = lock_entries(&turn).await;
        reconcile(&mut turn)?;
        let originals = preflight(&turn)?;
        let mut output = String::from("Latest edited turn (apply_patch only):\n");
        for (entry, original) in turn.manifest.entries.iter().zip(originals) {
            let (fingerprint, current) = inspect(&entry.path)?;
            if entry.before.matches(&fingerprint) {
                continue;
            }
            let letter = match (&original, &current) {
                (None, Some(_)) => 'A',
                (Some(_), None) => 'D',
                _ => 'M',
            };
            output.push_str(&format!("{letter} {}\n", entry.path.display()));
            if original == current {
                output.push_str("(file permissions changed)\n");
                continue;
            }
            let old = original.as_deref().unwrap_or_default();
            let new = current.as_deref().unwrap_or_default();
            match (std::str::from_utf8(old), std::str::from_utf8(new)) {
                (Ok(old), Ok(new)) => {
                    output.push_str(
                        &similar::TextDiff::from_lines(old, new)
                            .unified_diff()
                            .header("before", "after")
                            .to_string(),
                    );
                }
                _ => output.push_str("(binary contents changed)\n"),
            }
        }
        Ok(output.trim_end().to_string())
    }

    /// Validate every path and backup before restoring any file. Interrupted
    /// restorations can resume, but a conflicting current state never gets forced.
    pub async fn undo(&self) -> Result<Vec<PathBuf>, String> {
        let mut state = self.state.lock().await;
        let Some(mut turn) = self.latest()? else {
            return Err("No recorded patch edits to undo.".to_string());
        };
        if turn.manifest.status == TurnStatus::Unavailable {
            return Err(format!("refusing undo: {UNAVAILABLE}"));
        }
        let _guards = lock_entries(&turn).await;
        reconcile(&mut turn)?;
        let originals = preflight(&turn)?;
        turn.manifest.status = TurnStatus::Undoing;
        turn.save()?;
        for (entry, original) in turn.manifest.entries.iter().zip(originals) {
            let current = checked_fingerprint(entry)?;
            if entry.before.matches(&current) {
                continue;
            }
            if !entry.after.matches(&current) {
                return Err(conflict(&entry.path));
            }
            match original {
                Some(bytes) => {
                    let mode = match &entry.before {
                        Fingerprint::File { mode, .. } => *mode,
                        Fingerprint::Missing => None,
                    };
                    atomic_write(&entry.path, &bytes, mode, Some(entry))?;
                }
                None => {
                    std::fs::remove_file(&entry.path).map_err(|e| {
                        format!("cannot undo creation of {}: {e}", entry.path.display())
                    })?;
                    sync_directory(entry.path.parent().ok_or("undo path has no parent")?)?;
                }
            }
        }
        turn.manifest.status = TurnStatus::Undone;
        turn.save()?;
        state.active = None;
        Ok(turn
            .manifest
            .entries
            .into_iter()
            .map(|entry| entry.path)
            .collect())
    }
}

/// Held before taking file mutation locks, so parallel patch calls and undo use
/// the same lock order. Independent read/search/shell tools are unaffected.
pub(crate) struct Recording<'a> {
    journal: &'a UndoJournal,
    state: MutexGuard<'a, JournalState>,
}

impl Recording<'_> {
    pub(crate) fn prepare(
        &mut self,
        edits: &[(PathBuf, Option<&[u8]>)],
    ) -> Result<Option<&'static str>, String> {
        if self.journal.directory.is_none() {
            return Ok(None);
        }
        // These protections still apply after undo tracking has been disabled.
        for (path, _) in edits {
            let location = location(path)?;
            if self.journal.directory.as_ref().is_some_and(|root| {
                location.starts_with(root)
                    || path.starts_with(root)
                    || root
                        .canonicalize()
                        .is_ok_and(|root| location.starts_with(root))
            }) {
                return Err("patches cannot modify their own undo journal".to_string());
            }
            inspect(path)?;
        }
        if self.state.active.is_none() {
            self.state.active = Some(self.journal.new_turn()?);
        }
        let turn = self.state.active.as_mut().ok_or("undo turn unavailable")?;
        if turn.manifest.status == TurnStatus::Unavailable {
            return Ok(None);
        }
        if !recordable(turn) {
            return disable(turn);
        }
        for (path, content) in edits {
            let location = location(path)?;
            let (before, bytes) = inspect(path)?;
            let index = if let Some(index) = turn
                .manifest
                .entries
                .iter()
                .position(|entry| entry.location == location)
            {
                // Recheck the bytes read for this target too: a shell or user
                // edit can race the earlier whole-turn validation.
                if !turn.manifest.entries[index].after.matches(&before) {
                    return disable(turn);
                }
                index
            } else {
                let index = turn.manifest.entries.len();
                if let Some(bytes) = bytes {
                    write_new_private(&turn.backup(index), &bytes)?;
                }
                turn.manifest.entries.push(Entry {
                    path: path.clone(),
                    location,
                    before: before.clone(),
                    after: before.clone(),
                    pending: None,
                });
                index
            };
            let mode = match before {
                Fingerprint::File { mode, .. } => mode,
                Fingerprint::Missing => None,
            };
            turn.manifest.entries[index].pending = Some(match content {
                Some(bytes) => Fingerprint::File {
                    sha256: hash(bytes),
                    mode,
                },
                None => Fingerprint::Missing,
            });
        }
        turn.save()?;
        Ok(None)
    }

    pub(crate) fn finish(&mut self) -> Result<Option<&'static str>, String> {
        if let Some(turn) = &mut self.state.active
            && turn.manifest.status != TurnStatus::Unavailable
        {
            if !recordable(turn) {
                return disable(turn);
            }
            turn.save()?;
        }
        Ok(None)
    }
}

// A formatter or user edit must not be adopted as this turn's own work. Check
// all recorded paths, not just the next patch's spelling or target subset.
fn recordable(turn: &mut StoredTurn) -> bool {
    reconcile(turn).is_ok()
        && turn.manifest.entries.iter().all(|entry| {
            checked_fingerprint(entry).is_ok_and(|current| entry.after.matches(&current))
        })
}

fn disable(turn: &mut StoredTurn) -> Result<Option<&'static str>, String> {
    let previous = turn.manifest.status;
    turn.manifest.status = TurnStatus::Unavailable;
    // Persist before allowing further edits: a resumed session must also refuse
    // the whole turn, including otherwise unchanged files and older snapshots.
    if let Err(error) = turn.save() {
        turn.manifest.status = previous;
        return Err(error);
    }
    Ok(Some(UNAVAILABLE))
}

fn reconcile(turn: &mut StoredTurn) -> Result<(), String> {
    for entry in &mut turn.manifest.entries {
        if let Some(pending) = &entry.pending {
            let current = checked_fingerprint(entry)?;
            if !pending.matches(&current) && !entry.after.matches(&current) {
                return Err(conflict(&entry.path));
            }
            entry.after = current;
            entry.pending = None;
        }
    }
    Ok(())
}

fn preflight(turn: &StoredTurn) -> Result<Vec<Option<Vec<u8>>>, String> {
    turn.manifest
        .entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let current = checked_fingerprint(entry)?;
            let already_restored =
                turn.manifest.status == TurnStatus::Undoing && entry.before.matches(&current);
            if !(entry.after.matches(&current) || already_restored) {
                return Err(conflict(&entry.path));
            }
            turn.original(index)
        })
        .collect()
}

async fn lock_entries(turn: &StoredTurn) -> Vec<tokio::sync::OwnedMutexGuard<()>> {
    let mut paths: Vec<_> = turn
        .manifest
        .entries
        .iter()
        .map(|entry| &entry.location)
        .collect();
    paths.sort();
    paths.dedup();
    let mut guards = Vec::with_capacity(paths.len());
    for path in paths {
        guards.push(lock_file_for_mutation(path).await);
    }
    guards
}

fn checked_fingerprint(entry: &Entry) -> Result<Fingerprint, String> {
    if location(&entry.path)? != entry.location {
        return Err(conflict(&entry.path));
    }
    inspect(&entry.path).map(|(fingerprint, _)| fingerprint)
}

fn conflict(path: &Path) -> String {
    format!(
        "{} no longer matches Cupel's recorded state",
        path.display()
    )
}

fn hash(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut output = String::with_capacity(64);
    for byte in Sha256::digest(bytes) {
        let _ = write!(output, "{byte:02x}");
    }
    output
}

// Resolve existing regular leaves too, so case aliases use the filesystem's
// actual spelling. Missing leaves retain creation/deletion identity, and symlink
// leaves remain unsupported. Retargeted parents are still checked before undo.
fn location(path: &Path) -> Result<PathBuf, String> {
    if !path.is_absolute() {
        return Err(format!(
            "undo requires an absolute path: {}",
            path.display()
        ));
    }
    if std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_file()) {
        return path
            .canonicalize()
            .map_err(|e| format!("cannot resolve {} for undo: {e}", path.display()));
    }
    let mut parent = path.parent().ok_or("cannot snapshot filesystem root")?;
    let mut suffix = vec![
        path.file_name()
            .ok_or("invalid undo file path")?
            .to_os_string(),
    ];
    loop {
        match parent.canonicalize() {
            Ok(mut resolved) => {
                for component in suffix.iter().rev() {
                    resolved.push(component);
                }
                return Ok(resolved);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                suffix.push(
                    parent
                        .file_name()
                        .ok_or("cannot resolve undo path")?
                        .to_os_string(),
                );
                parent = parent.parent().ok_or("cannot resolve undo parent")?;
            }
            Err(e) => return Err(format!("cannot resolve {} for undo: {e}", path.display())),
        }
    }
}

fn inspect(path: &Path) -> Result<(Fingerprint, Option<Vec<u8>>), String> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok((Fingerprint::Missing, None));
        }
        Err(e) => return Err(format!("cannot inspect {} for undo: {e}", path.display())),
    };
    if !metadata.is_file() {
        return Err(format!(
            "undo supports regular files, not symlinks or special files: {}",
            path.display()
        ));
    }
    #[cfg(unix)]
    let mode = {
        use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
        if metadata.nlink() != 1 {
            return Err(format!(
                "undo does not support hard-linked files: {}",
                path.display()
            ));
        }
        Some(metadata.permissions().mode() & 0o7777)
    };
    #[cfg(not(unix))]
    let mode = Some(u32::from(metadata.permissions().readonly()));
    let bytes = std::fs::read(path)
        .map_err(|e| format!("cannot snapshot {} for undo: {e}", path.display()))?;
    Ok((
        Fingerprint::File {
            sha256: hash(&bytes),
            mode,
        },
        Some(bytes),
    ))
}

fn turn_directories(root: &Path) -> Result<Vec<(u64, PathBuf)>, String> {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("cannot list undo journal {}: {e}", root.display())),
    };
    let mut turns = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| format!("cannot list undo turns: {e}"))?;
        if let Some(number) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u64>().ok())
        {
            turns.push((number, entry.path()));
        }
    }
    turns.sort_by_key(|turn| std::cmp::Reverse(turn.0));
    Ok(turns)
}

fn create_private_directory(path: &Path, recursive: bool) -> Result<(), String> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(recursive);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt as _;
        builder.mode(0o700);
    }
    builder
        .create(path)
        .map_err(|e| format!("cannot create undo directory {}: {e}", path.display()))?;
    sync_directory(path.parent().ok_or("undo directory has no parent")?)
}

fn private_directory(path: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => Err(format!(
            "undo directory is not a regular directory: {}",
            path.display()
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => create_private_directory(path, true),
        Err(e) => Err(format!(
            "cannot inspect undo directory {}: {e}",
            path.display()
        )),
    }
}

fn open_private(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options.open(path)
}

fn write_new_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let mut file = open_private(path)
        .map_err(|e| format!("cannot create undo data {}: {e}", path.display()))?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|e| format!("cannot save undo data {}: {e}", path.display()))?;
    sync_directory(path.parent().ok_or("undo data has no parent")?)
}

fn sync_directory(path: &Path) -> Result<(), String> {
    std::fs::File::open(path)
        .and_then(|file| file.sync_all())
        .map_err(|e| format!("cannot sync undo directory {}: {e}", path.display()))
}

fn atomic_write(
    path: &Path,
    bytes: &[u8],
    mode: Option<u32>,
    expected: Option<&Entry>,
) -> Result<(), String> {
    let parent = path.parent().ok_or("undo target has no parent")?;
    std::fs::create_dir_all(parent)
        .map_err(|e| format!("cannot create undo parent {}: {e}", parent.display()))?;
    let mut allocated = None;
    for attempt in 0..100 {
        let temporary = parent.join(format!(
            ".cupel-undo-{}-{}-{attempt}.tmp",
            std::process::id(),
            cupel_core::types::now_ms()
        ));
        match open_private(&temporary) {
            Ok(file) => {
                allocated = Some((temporary, file));
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(format!("cannot stage undo for {}: {e}", path.display())),
        }
    }
    let (temporary, mut file) = allocated.ok_or("cannot allocate undo temporary file")?;
    let result = (|| {
        file.write_all(bytes)
            .map_err(|e| format!("cannot stage undo for {}: {e}", path.display()))?;
        if let Some(mode) = mode {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt as _;
                file.set_permissions(std::fs::Permissions::from_mode(mode))
                    .map_err(|e| {
                        format!("cannot restore permissions for {}: {e}", path.display())
                    })?;
            }
            #[cfg(not(unix))]
            {
                let mut permissions = std::fs::metadata(&temporary)
                    .map_err(|e| e.to_string())?
                    .permissions();
                permissions.set_readonly(mode != 0);
                std::fs::set_permissions(&temporary, permissions).map_err(|e| e.to_string())?;
            }
        }
        file.sync_all()
            .map_err(|e| format!("cannot sync undo for {}: {e}", path.display()))?;
        // Staging a large original can take time. Recheck immediately before the
        // replacement, not only before writing the temporary file.
        if let Some(entry) = expected
            && !entry.after.matches(&checked_fingerprint(entry)?)
        {
            return Err(conflict(path));
        }
        std::fs::rename(&temporary, path)
            .map_err(|e| format!("cannot restore {}: {e}", path.display()))?;
        sync_directory(parent)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

/// Frontend-neutral command handling, including a persisted context notice.
pub async fn execute_command(
    command: UndoCommand,
    agent: &mut cupel_agent::Agent,
    recorder: &mut SessionRecorder,
) -> Result<String, String> {
    if agent.state().is_streaming {
        return Err(
            "cannot inspect or undo edits while the agent is working (esc to abort first)"
                .to_string(),
        );
    }
    recorder.settle_hooks().await;
    let journal = recorder.undo();
    if command == UndoCommand::Changes {
        return journal.changes().await;
    }
    let paths = journal.undo().await?;
    let list = paths
        .iter()
        .map(|path| path.display().to_string())
        .collect::<Vec<_>>()
        .join("\n");
    let notice = format!(
        "The user ran /undo. Cupel restored the latest edited turn's apply_patch changes in:\n{list}\nThe conversation was not rewound. Shell side effects were not undone. Read these files again before editing them."
    );
    let message = cupel_agent::AgentMessage::user_text(notice);
    let persistence = recorder.record_checked(&message);
    agent.push_message(message);
    let mut output = format!(
        "Undid patch edits in {} path(s). Shell side effects were not undone.",
        paths.len()
    );
    if let Err(error) = persistence {
        output.push_str(&format!(
            "\nWarning: workspace restored, but the context notice could not be saved: {error}"
        ));
    }
    Ok(output)
}

#[must_use]
pub fn shared(transcript: Option<&Path>, cwd: &Path) -> Arc<UndoJournal> {
    Arc::new(UndoJournal::new(transcript, cwd))
}

#[cfg(test)]
#[path = "undo_tests.rs"]
mod tests;
