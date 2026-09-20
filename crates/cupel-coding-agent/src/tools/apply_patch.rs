//! The `apply_patch` tool: create, delete, rename and patch files with one `*** Begin
//! Patch` / `*** End Patch` envelope.
//!
//! A whole-file overwrite is the bluntest possible edit: the model re-emits every
//! line, and any slip is silently written out. A patch names its intent per file
//! (add, delete, update, move) and per hunk (context, removed, added), so the
//! tool can check each intent against the file before anything is written.
//!
//! The pieces: [`crate::tools::patch_parser`] turns the text into hunks,
//! [`crate::tools::patch_update`] applies update hunks to text, and this module is
//! the I/O shell around them. Unlike Codex's standalone `apply_patch` binary, which
//! writes hunk by hunk and leaves earlier hunks applied when a later one fails, this
//! tool follows Codex's tool handler: verify every hunk first (all reads, no writes),
//! then apply. A patch that does not match the files changes nothing.
//!
//! Every file the patch touches is locked for the whole verify-and-apply cycle.

use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use cupel_agent::types::{AgentTool, AgentToolResult, ToolError, ToolUpdateFn};
use cupel_core::types::{TextContent, ToolResultContent};

use crate::search::resolve_to_root;
use crate::tools::file_queue::lock_file_for_mutation;
use crate::tools::patch_parser::{Hunk, parse_patch};
use crate::tools::patch_update::apply_chunks;
use crate::tools::text_diff::{
    detect_line_ending, generate_diff_string, normalize_to_lf, restore_line_endings, strip_bom,
};

const DIFF_CONTEXT_LINES: usize = 4;
const NEW_FILE_PREVIEW_LINES: usize = 20;

/// What the model reads about the format. cupel cannot constrain decoding
/// with a grammar the way the OpenAI API does for Codex, so the description
/// has to teach the envelope itself (adapted from Codex's original
/// apply_patch instructions).
const DESCRIPTION: &str = "Create, delete, rename, or update files with one patch. The input is \
a text envelope (not JSON):\n\
\n\
*** Begin Patch\n\
*** Add File: docs/new.md\n\
+first line of the new file\n\
+second line\n\
*** Update File: src/app.py\n\
*** Move to: src/main.py\n\
@@ def greet():\n\
 unchanged context line (prefixed with a space)\n\
-old line\n\
+new line\n\
*** Delete File: obsolete.txt\n\
*** End Patch\n\
\n\
Every file section starts with '*** Add File: <path>' (every following line is a + line with \
the initial contents), '*** Delete File: <path>' (nothing follows), or '*** Update File: \
<path>'. '*** Move to: <new path>' may directly follow an Update header to rename the file. \
An update holds one or more hunks; each starts with '@@' (optionally followed by the \
enclosing function or class line to locate it) and every line inside starts with a space \
(context), '-' (removed), or '+' (added). Show 3 lines of context above and below each \
change so the hunk is unique in the file; if it is still ambiguous, add '@@ <function>' \
markers. Put '*** End of File' after a hunk that ends at the end of the file. A hunk with \
only '+' lines and no context is appended to the end of the file. Paths are relative to \
the working directory. Lines are matched exactly, then ignoring trailing whitespace. If any \
hunk does not match, the whole patch is rejected and no file is written.";

#[derive(Debug, Deserialize)]
struct ApplyPatchArgs {
    input: String,
}

pub struct ApplyPatchTool {
    cwd: PathBuf,
}

impl ApplyPatchTool {
    #[must_use]
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self { cwd: cwd.into() }
    }

    /// Every absolute path the patch writes or removes, deduplicated the
    /// way the file queue keys its locks (canonical path when the file
    /// exists) and sorted. Two hunks on one file are refused up front, as
    /// in Codex: applying them in sequence would make the second one
    /// operate on the first one's result, which the patch did not say.
    fn lock_keys(&self, hunks: &[Hunk]) -> Result<Vec<PathBuf>, ToolError> {
        let mut targets: Vec<PathBuf> = Vec::new();
        let mut keys: Vec<PathBuf> = Vec::new();
        for hunk in hunks {
            let source = match hunk {
                Hunk::AddFile { path, .. }
                | Hunk::DeleteFile { path }
                | Hunk::UpdateFile { path, .. } => path,
            };
            let target = queue_key(&resolve_to_root(source, &self.cwd));
            if targets.contains(&target) {
                return Err(format!("multiple operations target {source}").into());
            }
            targets.push(target.clone());
            keys.push(target);
            if let Hunk::UpdateFile {
                move_path: Some(destination),
                ..
            } = hunk
            {
                keys.push(queue_key(&resolve_to_root(destination, &self.cwd)));
            }
        }
        keys.sort();
        keys.dedup();
        Ok(keys)
    }

    /// Read and match, never write. Errors here mean the patch does not fit
    /// the files as they are.
    async fn verify(&self, hunk: &Hunk) -> Result<Change, ToolError> {
        match hunk {
            Hunk::AddFile { path, contents } => Ok(Change::Add {
                path: resolve_to_root(path, &self.cwd),
                display: path.clone(),
                content: contents.clone(),
            }),
            Hunk::DeleteFile { path } => {
                let absolute = resolve_to_root(path, &self.cwd);
                let metadata = tokio::fs::metadata(&absolute)
                    .await
                    .map_err(|e| format!("Failed to delete file {path}: {e}"))?;
                if metadata.is_dir() {
                    return Err(format!("Failed to delete file {path}: path is a directory").into());
                }
                Ok(Change::Delete {
                    path: absolute,
                    display: path.clone(),
                })
            }
            Hunk::UpdateFile {
                path,
                move_path,
                chunks,
            } => {
                let absolute = resolve_to_root(path, &self.cwd);
                let raw = tokio::fs::read_to_string(&absolute)
                    .await
                    .map_err(|e| format!("Failed to read file to update {path}: {e}"))?;
                // Match in BOM-free, LF-normalized space; restore both on
                // write.
                let (bom, text) = strip_bom(&raw);
                let ending = detect_line_ending(text);
                let old_text = normalize_to_lf(text);
                let new_text = apply_chunks(&old_text, chunks, path)?;
                let content = format!("{bom}{}", restore_line_endings(&new_text, ending));
                Ok(Change::Update {
                    path: absolute,
                    display: path.clone(),
                    move_to: move_path.as_ref().map(|destination| {
                        (resolve_to_root(destination, &self.cwd), destination.clone())
                    }),
                    old_text,
                    new_text,
                    content,
                })
            }
        }
    }
}

/// One verified file operation: all reading and matching is done, only
/// the write is left. `display` is the path as the patch spelled it; the
/// model and the transcript see that spelling, never the absolute path.
enum Change {
    Add {
        path: PathBuf,
        display: String,
        content: String,
    },
    Delete {
        path: PathBuf,
        display: String,
    },
    Update {
        path: PathBuf,
        display: String,
        move_to: Option<(PathBuf, String)>,
        old_text: String,
        new_text: String,
        content: String,
    },
}

#[async_trait::async_trait]
impl AgentTool for ApplyPatchTool {
    fn name(&self) -> &str {
        "apply_patch"
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "input": {
                    "type": "string",
                    "description": "The full patch text, from '*** Begin Patch' to '*** End Patch'"
                }
            },
            "required": ["input"]
        })
    }

    /// `apply_patch A new.txt, M src/main.rs, D old.txt` - the diff below the
    /// header shows what changed, so the header lists where, git-style.
    fn describe_call(&self, args: &Value) -> String {
        let Some(hunks) = args
            .get("input")
            .and_then(Value::as_str)
            .and_then(|input| parse_patch(input).ok())
            .filter(|hunks| !hunks.is_empty())
        else {
            return self.name().to_string();
        };
        const SHOWN: usize = 3;
        let mut listed: Vec<String> = hunks
            .iter()
            .take(SHOWN)
            .map(|hunk| format!("{} {}", status_letter(hunk), hunk.path()))
            .collect();
        if hunks.len() > SHOWN {
            listed.push(format!("(+{} more)", hunks.len() - SHOWN));
        }
        format!("apply_patch {}", listed.join(", "))
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        args: Value,
        cancel: CancellationToken,
        _on_update: Option<ToolUpdateFn>,
    ) -> Result<AgentToolResult, ToolError> {
        let args: ApplyPatchArgs = serde_json::from_value(args)?;
        let hunks = parse_patch(&args.input).map_err(|e| e.to_string())?;
        if hunks.is_empty() {
            return Err("No files were modified.".into());
        }

        // Lock every file the patch touches, in one global order, before
        // reading anything: the verify step must see the same bytes the
        // apply step overwrites, and two patches on overlapping files must
        // not deadlock (sorted acquisition means no cycles).
        let _guards = lock_all(&self.lock_keys(&hunks)?).await;
        if cancel.is_cancelled() {
            return Err("Operation aborted".into());
        }

        let mut changes: Vec<Change> = Vec::with_capacity(hunks.len());
        for hunk in &hunks {
            changes.push(self.verify(hunk).await?);
            if cancel.is_cancelled() {
                return Err("Operation aborted".into());
            }
        }

        let mut diff = String::new();
        let (mut added, mut modified, mut deleted) = (Vec::new(), Vec::new(), Vec::new());
        for change in &changes {
            apply(change).await?;
            match change {
                Change::Add {
                    display, content, ..
                } => {
                    diff.push_str(&added_file_diff(display, content));
                    added.push(display.clone());
                }
                Change::Delete { display, .. } => {
                    diff.push_str(&format!("D {display}\n"));
                    deleted.push(display.clone());
                }
                Change::Update {
                    display,
                    move_to,
                    old_text,
                    new_text,
                    ..
                } => {
                    let target = move_to
                        .as_ref()
                        .map_or(display.clone(), |(_, destination)| destination.clone());
                    diff.push_str(&match move_to {
                        Some((_, destination)) => format!("M {display} -> {destination}\n"),
                        None => format!("M {display}\n"),
                    });
                    diff.push_str(
                        &generate_diff_string(old_text, new_text, DIFF_CONTEXT_LINES).diff,
                    );
                    diff.push('\n');
                    modified.push(target);
                }
            }
        }

        // Codex's summary, verbatim: the models know it.
        let mut summary = String::from("Success. Updated the following files:");
        for (letter, paths) in [('A', &added), ('M', &modified), ('D', &deleted)] {
            for path in paths {
                summary.push_str(&format!("\n{letter} {path}"));
            }
        }
        Ok(AgentToolResult {
            content: vec![ToolResultContent::Text(TextContent::plain(summary))],
            // The diff rides in details for the transcript; the model only
            // needs the summary (it wrote the patch).
            details: Some(json!({ "diff": diff.trim_end() })),
            terminate: false,
        })
    }
}

/// The file queue's key: symlinks and `./` resolved when the file exists,
/// the plain absolute path otherwise (a file about to be created).
fn queue_key(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

async fn lock_all(keys: &[PathBuf]) -> Vec<tokio::sync::OwnedMutexGuard<()>> {
    let mut guards = Vec::with_capacity(keys.len());
    for key in keys {
        guards.push(lock_file_for_mutation(key).await);
    }
    guards
}

async fn write_creating_parents(
    path: &Path,
    display: &str,
    content: &str,
) -> Result<(), ToolError> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("Failed to create parent directories for {display}: {e}"))?;
    }
    tokio::fs::write(path, content)
        .await
        .map_err(|e| format!("Failed to write file {display}: {e}"))?;
    Ok(())
}

/// The write step. Like Codex, an I/O failure here leaves the changes
/// before it in place; the error names the file, and the model can read
/// it back.
async fn apply(change: &Change) -> Result<(), ToolError> {
    match change {
        Change::Add {
            path,
            display,
            content,
        } => write_creating_parents(path, display, content).await,
        Change::Delete { path, display } => tokio::fs::remove_file(path)
            .await
            .map_err(|e| format!("Failed to delete file {display}: {e}").into()),
        Change::Update {
            path,
            display,
            move_to: None,
            content,
            ..
        } => tokio::fs::write(path, content)
            .await
            .map_err(|e| format!("Failed to write file {display}: {e}").into()),
        Change::Update {
            path,
            display,
            move_to: Some((destination, destination_display)),
            content,
            ..
        } => {
            // A rename is "write the new content there, remove the original
            // here": no `rename(2)`, so the destination gets the patched
            // content even across filesystems, and an existing destination
            // is overwritten.
            write_creating_parents(destination, destination_display, content).await?;
            tokio::fs::remove_file(path)
                .await
                .map_err(|e| format!("Failed to remove original {display}: {e}"))?;
            Ok(())
        }
    }
}

fn status_letter(hunk: &Hunk) -> char {
    match hunk {
        Hunk::AddFile { .. } => 'A',
        Hunk::DeleteFile { .. } => 'D',
        Hunk::UpdateFile { .. } => 'M',
    }
}

/// `A path` plus the first lines of the new file as `+` rows.
fn added_file_diff(display: &str, content: &str) -> String {
    let full = generate_diff_string("", content, DIFF_CONTEXT_LINES).diff;
    let mut lines = full.lines();
    let mut out = format!("A {display}\n");
    for line in lines.by_ref().take(NEW_FILE_PREVIEW_LINES) {
        out.push_str(line);
        out.push('\n');
    }
    let hidden = lines.count();
    if hidden > 0 {
        out.push_str(&format!("  ... ({hidden} more lines)\n"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh directory per test so file assertions cannot see each
    /// other's leftovers.
    fn temp_root(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("cupel-apply-patch-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(root: &Path, relative: &str, content: &str) {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn read(root: &Path, relative: &str) -> String {
        std::fs::read_to_string(root.join(relative)).unwrap()
    }

    async fn run(root: &Path, patch: &str) -> Result<AgentToolResult, ToolError> {
        ApplyPatchTool::new(root)
            .execute(
                "call_1",
                json!({"input": patch}),
                CancellationToken::new(),
                None,
            )
            .await
    }

    fn text_of(result: &AgentToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|c| match c {
                ToolResultContent::Text(t) => Some(t.text.clone()),
                ToolResultContent::Image(_) => None,
            })
            .collect()
    }

    fn diff_of(result: &AgentToolResult) -> String {
        result.details.as_ref().unwrap()["diff"]
            .as_str()
            .unwrap()
            .to_string()
    }

    #[test]
    fn describe_call_lists_the_files_git_style() {
        let tool = ApplyPatchTool::new("/tmp");
        let patch = "*** Begin Patch\n*** Add File: nested/new.txt\n+created\n*** Delete File: delete.txt\n*** Update File: modify.txt\n*** Move to: moved.txt\n@@\n-line2\n+changed\n*** End Patch";
        assert_eq!(
            tool.describe_call(&json!({"input": patch})),
            "apply_patch A nested/new.txt, D delete.txt, M moved.txt"
        );
        let four = "*** Begin Patch\n*** Delete File: a\n*** Delete File: b\n*** Delete File: c\n*** Delete File: d\n*** End Patch";
        assert_eq!(
            tool.describe_call(&json!({"input": four})),
            "apply_patch D a, D b, D c, (+1 more)"
        );
        assert_eq!(
            tool.describe_call(&json!({"input": "garbage"})),
            "apply_patch"
        );
        assert_eq!(tool.describe_call(&json!({})), "apply_patch");
    }

    #[tokio::test]
    async fn multiple_operations_in_one_patch() {
        // Codex fixture 002.
        let root = temp_root("multiple");
        write(&root, "delete.txt", "obsolete\n");
        write(&root, "modify.txt", "line1\nline2\n");
        let result = run(
            &root,
            "*** Begin Patch\n*** Add File: nested/new.txt\n+created\n*** Delete File: delete.txt\n*** Update File: modify.txt\n@@\n-line2\n+changed\n*** End Patch",
        )
        .await
        .unwrap();
        assert_eq!(read(&root, "nested/new.txt"), "created\n");
        assert!(!root.join("delete.txt").exists());
        assert_eq!(read(&root, "modify.txt"), "line1\nchanged\n");
        assert_eq!(
            text_of(&result),
            "Success. Updated the following files:\nA nested/new.txt\nM modify.txt\nD delete.txt"
        );
        let diff = diff_of(&result);
        assert_eq!(
            diff,
            "A nested/new.txt\n+1 created\nD delete.txt\nM modify.txt\n 1 line1\n-2 line2\n+2 changed",
            "got:\n{diff}"
        );
    }

    #[tokio::test]
    async fn move_creates_directories_and_overwrites_the_destination() {
        // Codex fixtures 004 and 010.
        let root = temp_root("move");
        write(&root, "old/name.txt", "from\n");
        write(&root, "old/other.txt", "unrelated file\n");
        write(&root, "renamed/dir/name.txt", "existing\n");
        let result = run(
            &root,
            "*** Begin Patch\n*** Update File: old/name.txt\n*** Move to: renamed/dir/name.txt\n@@\n-from\n+new\n*** End Patch",
        )
        .await
        .unwrap();
        assert!(!root.join("old/name.txt").exists());
        assert_eq!(read(&root, "old/other.txt"), "unrelated file\n");
        assert_eq!(read(&root, "renamed/dir/name.txt"), "new\n");
        assert_eq!(
            text_of(&result),
            "Success. Updated the following files:\nM renamed/dir/name.txt"
        );
        assert!(diff_of(&result).starts_with("M old/name.txt -> renamed/dir/name.txt\n"));
    }

    #[tokio::test]
    async fn add_overwrites_an_existing_file() {
        // Codex fixture 011.
        let root = temp_root("overwrite");
        write(&root, "duplicate.txt", "old content\n");
        run(
            &root,
            "*** Begin Patch\n*** Add File: duplicate.txt\n+new content\n*** End Patch",
        )
        .await
        .unwrap();
        assert_eq!(read(&root, "duplicate.txt"), "new content\n");
    }

    #[tokio::test]
    async fn missing_files_and_directories_are_rejected_before_any_write() {
        // Codex fixtures 007, 009, 012 and (with a twist) 015: Codex's
        // binary leaves created.txt behind, this tool verifies first.
        let root = temp_root("reject");
        write(&root, "foo.txt", "stable\n");
        write(&root, "dir/foo.txt", "stable\n");
        let err = run(
            &root,
            "*** Begin Patch\n*** Delete File: missing.txt\n*** End Patch",
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string()
                .starts_with("Failed to delete file missing.txt: "),
            "{err}"
        );
        let err = run(
            &root,
            "*** Begin Patch\n*** Update File: missing.txt\n@@\n-old\n+new\n*** End Patch",
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string()
                .starts_with("Failed to read file to update missing.txt: "),
            "{err}"
        );
        let err = run(
            &root,
            "*** Begin Patch\n*** Delete File: dir\n*** End Patch",
        )
        .await
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "Failed to delete file dir: path is a directory"
        );
        let err = run(
            &root,
            "*** Begin Patch\n*** Add File: created.txt\n+hello\n*** Update File: missing.txt\n@@\n-old\n+new\n*** End Patch",
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("missing.txt"), "{err}");
        assert!(
            !root.join("created.txt").exists(),
            "verify-then-apply: nothing written"
        );
        assert_eq!(read(&root, "foo.txt"), "stable\n");
    }

    #[tokio::test]
    async fn context_mismatch_and_empty_patch_are_errors() {
        // Codex fixtures 005 and 006.
        let root = temp_root("mismatch");
        write(&root, "modify.txt", "line1\nline2\n");
        let err = run(
            &root,
            "*** Begin Patch\n*** Update File: modify.txt\n@@\n-missing\n+changed\n*** End Patch",
        )
        .await
        .unwrap_err();
        assert_eq!(
            err.to_string(),
            "Failed to find expected lines in modify.txt:\nmissing"
        );
        assert_eq!(read(&root, "modify.txt"), "line1\nline2\n");
        let err = run(&root, "*** Begin Patch\n*** End Patch")
            .await
            .unwrap_err();
        assert_eq!(err.to_string(), "No files were modified.");
        let err = run(&root, "not a patch").await.unwrap_err();
        assert_eq!(
            err.to_string(),
            "invalid patch: The first line of the patch must be '*** Begin Patch'"
        );
    }

    #[tokio::test]
    async fn two_hunks_on_one_file_are_refused() {
        let root = temp_root("duplicate");
        write(&root, "a.txt", "one\n");
        let err = run(
            &root,
            "*** Begin Patch\n*** Update File: a.txt\n@@\n-one\n+two\n*** Update File: ./a.txt\n@@\n-two\n+three\n*** End Patch",
        )
        .await
        .unwrap_err();
        assert_eq!(err.to_string(), "multiple operations target ./a.txt");
        assert_eq!(read(&root, "a.txt"), "one\n");
    }

    #[tokio::test]
    async fn crlf_and_bom_survive_an_update() {
        // Codex fixture 023, plus the BOM round trip edit already does.
        let root = temp_root("crlf");
        write(&root, "lines.txt", "\u{FEFF}one\r\ntwo\r\nthree\r\n");
        run(
            &root,
            "*** Begin Patch\n*** Update File: lines.txt\n@@\n-one\n+ONE\n two\n+between\n three\n*** End Patch",
        )
        .await
        .unwrap();
        assert_eq!(
            read(&root, "lines.txt"),
            "\u{FEFF}ONE\r\ntwo\r\nbetween\r\nthree\r\n"
        );
    }

    #[tokio::test]
    async fn long_new_files_are_previewed_in_the_diff() {
        let root = temp_root("preview");
        let body = (1..=25)
            .map(|i| format!("+line {i}\n"))
            .collect::<Vec<_>>()
            .concat();
        let result = run(
            &root,
            &format!("*** Begin Patch\n*** Add File: big.txt\n{body}*** End Patch"),
        )
        .await
        .unwrap();
        let diff = diff_of(&result);
        assert!(diff.starts_with("A big.txt\n+ 1 line 1\n"), "got:\n{diff}");
        assert!(
            diff.ends_with("+20 line 20\n  ... (5 more lines)"),
            "got:\n{diff}"
        );
        assert_eq!(read(&root, "big.txt").lines().count(), 25);
    }
}
