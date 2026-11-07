use super::*;
use crate::tools::apply_patch::ApplyPatchTool;
use cupel_agent::types::{AgentTool as _, AgentToolResult, ToolError};
use tokio_util::sync::CancellationToken;

struct Fixture {
    root: PathBuf,
    cwd: PathBuf,
    transcript: PathBuf,
    journal: Arc<UndoJournal>,
}

impl Fixture {
    fn new(name: &str) -> Self {
        let root = std::env::temp_dir().join(format!("cupel-undo-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("project")).unwrap();
        let root = root.canonicalize().unwrap();
        let cwd = root.join("project");
        let transcript = root.join("home/sessions/project/cupel-test.jsonl");
        let journal = shared(Some(&transcript), &cwd);
        Self {
            root,
            cwd,
            transcript,
            journal,
        }
    }

    fn write(&self, path: &str, bytes: impl AsRef<[u8]>) {
        let path = self.cwd.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    fn read(&self, path: &str) -> Vec<u8> {
        std::fs::read(self.cwd.join(path)).unwrap()
    }

    async fn patch(&self, body: &str) -> Result<AgentToolResult, ToolError> {
        ApplyPatchTool::new(&self.cwd)
            .with_undo(Arc::clone(&self.journal))
            .execute(
                "patch",
                serde_json::json!({"input": format!("*** Begin Patch\n{body}*** End Patch")}),
                CancellationToken::new(),
                None,
            )
            .await
    }

    async fn update(&self, path: &str, before: &str, after: &str) {
        self.patch(&format!(
            "*** Update File: {path}\n@@\n-{before}\n+{after}\n"
        ))
        .await
        .unwrap();
    }

    fn resumed(&self) -> Arc<UndoJournal> {
        shared(Some(&self.transcript), &self.cwd)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[tokio::test]
async fn restores_dirty_original_bytes_not_git_or_normalized_patch_text() {
    let fixture = Fixture::new("dirty");
    let original = b"\xef\xbb\xbfone\r\nmy uncommitted work";
    fixture.write("dirty.txt", original);
    fixture.write("untouched.txt", "mine\n");
    fixture.update("dirty.txt", "one", "agent").await;
    fixture.journal.undo().await.unwrap();
    assert_eq!(fixture.read("dirty.txt"), original);
    assert_eq!(fixture.read("untouched.txt"), b"mine\n");
}

#[tokio::test]
async fn one_turn_restores_creation_deletion_renames_and_overwritten_destinations() {
    let fixture = Fixture::new("operations");
    fixture.write("deleted.bin", [0, 255, 1, 128]);
    fixture.write("source", "source\n");
    fixture.write("destination", "my destination\n");
    fixture.write("overwrite", "my original\n");
    fixture.patch("*** Add File: nested/new\n+new\n*** Delete File: deleted.bin\n*** Update File: source\n*** Move to: destination\n@@\n-source\n+moved\n*** Add File: overwrite\n+replacement\n").await.unwrap();
    assert_eq!(fixture.read("destination"), b"moved\n");
    fixture.resumed().undo().await.unwrap();
    assert!(!fixture.cwd.join("nested/new").exists());
    assert_eq!(fixture.read("deleted.bin"), [0, 255, 1, 128]);
    assert_eq!(fixture.read("source"), b"source\n");
    assert_eq!(fixture.read("destination"), b"my destination\n");
    assert_eq!(fixture.read("overwrite"), b"my original\n");
}

#[tokio::test]
async fn repeated_patches_share_one_original_and_undo_walks_back_edited_turns() {
    let fixture = Fixture::new("turns");
    fixture.write("a", "mine\n");
    fixture.update("a", "mine", "first").await;
    fixture.update("a", "first", "second").await;
    let first = fixture.journal.latest().unwrap().unwrap();
    assert_eq!(first.manifest.entries.len(), 1);
    assert_eq!(first.original(0).unwrap().unwrap(), b"mine\n");
    fixture.journal.begin_turn().await;
    fixture.update("a", "second", "third").await;
    let resumed = fixture.resumed();
    resumed.undo().await.unwrap();
    assert_eq!(fixture.read("a"), b"second\n");
    resumed.undo().await.unwrap();
    assert_eq!(fixture.read("a"), b"mine\n");
    assert!(resumed.undo().await.unwrap_err().contains("No recorded"));
    resumed.begin_turn().await;
    assert!(resumed.latest().unwrap().is_none());
}

#[tokio::test]
async fn changes_shows_full_recorded_diffs_without_modifying_the_workspace() {
    let fixture = Fixture::new("changes");
    fixture.write("a", "mine\n");
    fixture.write("unrelated", "secret dirty file\n");
    fixture.write("binary", [255, 0]);
    fixture
        .patch("*** Update File: a\n@@\n-mine\n+agent\n*** Delete File: binary\n")
        .await
        .unwrap();
    let output = fixture.resumed().changes().await.unwrap();
    assert!(output.contains("-mine\n+agent"), "{output}");
    assert!(output.contains("binary contents changed"));
    assert!(!output.contains("unrelated"));
    assert_eq!(fixture.read("a"), b"agent\n");
    assert!(!fixture.cwd.join("binary").exists());
}

#[tokio::test]
async fn any_newer_file_edit_blocks_the_entire_turn_and_can_be_retried() {
    let fixture = Fixture::new("conflict");
    fixture.write("a", "mine\n");
    fixture.write("b", "before\n");
    fixture
        .patch("*** Update File: a\n@@\n-mine\n+agent\n*** Update File: b\n@@\n-before\n+after\n")
        .await
        .unwrap();
    fixture.write("b", "my later edit\n");
    assert!(
        fixture
            .resumed()
            .undo()
            .await
            .unwrap_err()
            .contains("recorded state")
    );
    assert!(fixture.journal.changes().await.is_err());
    assert_eq!(
        fixture.read("a"),
        b"agent\n",
        "preflight must not restore the earlier path"
    );
    assert_eq!(fixture.read("b"), b"my later edit\n");
    fixture.write("b", "after\n");
    fixture.resumed().undo().await.unwrap();
    assert_eq!(fixture.read("a"), b"mine\n");
    assert_eq!(fixture.read("b"), b"before\n");
}

#[tokio::test]
async fn later_creation_and_deletion_are_conflicts_too() {
    let fixture = Fixture::new("existence");
    fixture.write("deleted", "original\n");
    fixture
        .patch("*** Delete File: deleted\n*** Add File: created\n+agent\n")
        .await
        .unwrap();
    fixture.write("deleted", "user recreated\n");
    assert!(fixture.journal.undo().await.is_err());
    assert_eq!(fixture.read("created"), b"agent\n");
    std::fs::remove_file(fixture.cwd.join("deleted")).unwrap();
    std::fs::remove_file(fixture.cwd.join("created")).unwrap();
    assert!(fixture.journal.undo().await.is_err());
    assert!(!fixture.cwd.join("deleted").exists());
}

#[tokio::test]
async fn formatter_changes_allow_patching_but_disable_undo_with_one_warning() {
    let fixture = Fixture::new("between");
    fixture.write("a", "mine\n");
    fixture.update("a", "mine", "agent").await;
    fixture.write("a", "    agent\n");
    let result = fixture
        .patch("*** Update File: a\n@@\n-    agent\n+    agent again\n")
        .await
        .unwrap();
    let summary = result_text(&result);
    assert_eq!(summary.matches(UNAVAILABLE).count(), 1, "{summary}");
    assert!(summary.starts_with("Success."), "{summary}");
    assert!(!summary.contains("refusing undo"), "{summary}");
    assert!(
        result.details.unwrap()["diff"]
            .as_str()
            .unwrap()
            .starts_with("Warning:")
    );
    assert_eq!(fixture.read("a"), b"    agent again\n");

    let turn = fixture.resumed().latest().unwrap().unwrap();
    assert_eq!(turn.manifest.status, TurnStatus::Unavailable);
    assert_eq!(turn.original(0).unwrap().unwrap(), b"mine\n");
    let result = fixture
        .patch("*** Update File: a\n@@\n-    agent again\n+final\n")
        .await
        .unwrap();
    assert!(!result_text(&result).contains(UNAVAILABLE));
    assert_eq!(fixture.journal.changes().await.unwrap_err(), UNAVAILABLE);
    assert!(
        fixture
            .resumed()
            .undo()
            .await
            .unwrap_err()
            .starts_with("refusing undo:")
    );
    assert_eq!(fixture.read("a"), b"final\n");
}

fn result_text(result: &AgentToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|content| match content {
            cupel_core::types::ToolResultContent::Text(text) => Some(text.text.as_str()),
            cupel_core::types::ToolResultContent::Image(_) => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[tokio::test]
async fn conflicts_in_other_recorded_files_disable_the_whole_turn() {
    let fixture = Fixture::new("other-conflict");
    fixture.write("a", "mine\n");
    fixture.write("b", "original\n");
    fixture.update("a", "mine", "agent").await;
    fixture.update("b", "original", "edited").await;
    fixture.write("a", "formatted\n");
    let result = fixture
        .patch("*** Update File: b\n@@\n-edited\n+final\n")
        .await
        .unwrap();
    assert!(result_text(&result).contains(UNAVAILABLE));
    assert_eq!(fixture.read("a"), b"formatted\n");
    assert_eq!(fixture.read("b"), b"final\n");
    assert!(fixture.resumed().undo().await.is_err());
    assert_eq!(fixture.read("a"), b"formatted\n");
    assert_eq!(fixture.read("b"), b"final\n");
}

#[tokio::test]
async fn unavailable_turns_are_persistent_barriers_not_skipped_undo_history() {
    let fixture = Fixture::new("unavailable-barrier");
    fixture.write("a", "original\n");
    fixture.update("a", "original", "first").await;
    fixture.journal.begin_turn().await;
    fixture.update("a", "first", "first").await;
    fixture.write("a", "formatted\n");
    fixture.update("a", "formatted", "final").await;
    // Even restoring the old fingerprint must not re-enable an invalidated turn.
    fixture.write("a", "first\n");
    assert!(
        fixture
            .resumed()
            .undo()
            .await
            .unwrap_err()
            .contains(UNAVAILABLE)
    );
    fixture.journal.begin_turn().await;
    let result = fixture
        .patch("*** Update File: a\n@@\n-first\n+third\n")
        .await
        .unwrap();
    assert!(!result_text(&result).contains(UNAVAILABLE));
    let resumed = fixture.resumed();
    resumed.undo().await.unwrap();
    assert_eq!(fixture.read("a"), b"first\n");
    assert!(resumed.undo().await.unwrap_err().contains(UNAVAILABLE));
    assert_eq!(fixture.read("a"), b"first\n");
}

#[tokio::test]
async fn stale_patch_hunks_still_fail_without_modifying_the_file() {
    let fixture = Fixture::new("stale-hunks");
    fixture.write("a", "mine\n");
    fixture.update("a", "mine", "agent").await;
    fixture.write("a", "formatted\n");
    let error = fixture
        .patch("*** Update File: a\n@@\n-agent\n+new\n")
        .await
        .unwrap_err();
    assert!(!error.to_string().contains("refusing undo"));
    assert_eq!(fixture.read("a"), b"formatted\n");
    assert_eq!(
        fixture.journal.latest().unwrap().unwrap().manifest.status,
        TurnStatus::Recording
    );
}

#[tokio::test]
async fn case_aliases_share_snapshots_and_cannot_bypass_external_edit_detection() {
    let fixture = Fixture::new("case-alias");
    fixture.write("file", "mine\n");
    if !fixture.cwd.join("FILE").exists() {
        return; // This filesystem treats the names as distinct files.
    }
    fixture.update("file", "mine", "agent").await;
    let result = fixture
        .patch("*** Update File: FILE\n@@\n-agent\n+again\n")
        .await
        .unwrap();
    assert!(!result_text(&result).contains(UNAVAILABLE));
    assert_eq!(
        fixture
            .journal
            .latest()
            .unwrap()
            .unwrap()
            .manifest
            .entries
            .len(),
        1
    );
    fixture.resumed().undo().await.unwrap();
    assert_eq!(fixture.read("file"), b"mine\n");

    fixture.journal.begin_turn().await;
    fixture.update("file", "mine", "agent").await;
    fixture.write("file", "formatted\n");
    let result = fixture
        .patch("*** Update File: FILE\n@@\n-formatted\n+final\n")
        .await
        .unwrap();
    assert!(result_text(&result).contains(UNAVAILABLE));
    assert!(fixture.resumed().undo().await.is_err());
    assert_eq!(fixture.read("file"), b"final\n");
}

#[tokio::test]
async fn case_distinct_files_remain_independently_undoable() {
    let fixture = Fixture::new("case-distinct");
    fixture.write("file", "lower\n");
    if fixture.cwd.join("FILE").exists() {
        return; // Case aliases are exercised separately on insensitive filesystems.
    }
    fixture.write("FILE", "upper\n");
    fixture.update("file", "lower", "edited lower").await;
    fixture.update("FILE", "upper", "edited upper").await;
    assert_eq!(fixture.resumed().undo().await.unwrap().len(), 2);
    assert_eq!(fixture.read("file"), b"lower\n");
    assert_eq!(fixture.read("FILE"), b"upper\n");
}

#[tokio::test]
async fn damaged_or_missing_backups_refuse_before_any_restoration() {
    let fixture = Fixture::new("damaged");
    fixture.write("a", "before\n");
    fixture.write("b", "before\n");
    fixture.update("a", "before", "after").await;
    fixture.update("b", "before", "after").await;
    let turn = fixture.journal.latest().unwrap().unwrap();
    std::fs::write(turn.backup(1), "corrupt").unwrap();
    assert!(
        fixture
            .journal
            .undo()
            .await
            .unwrap_err()
            .contains("damaged")
    );
    assert_eq!(fixture.read("a"), b"after\n");
    std::fs::remove_file(turn.backup(1)).unwrap();
    assert!(
        fixture
            .journal
            .undo()
            .await
            .unwrap_err()
            .contains("cannot read")
    );
    assert_eq!(fixture.read("a"), b"after\n");
}

#[tokio::test]
async fn partial_patches_reconcile_only_the_previous_or_intended_contents() {
    let fixture = Fixture::new("partial");
    fixture.write("a", "before\n");
    fixture.write("b", "before\n");
    {
        let mut recording = fixture.journal.lock().await;
        recording
            .prepare(&[
                (fixture.cwd.join("a"), Some(b"after\n")),
                (fixture.cwd.join("b"), Some(b"after\n")),
            ])
            .unwrap();
        fixture.write("a", "after\n");
        recording.finish().unwrap();
    }
    fixture.resumed().undo().await.unwrap();
    assert_eq!(fixture.read("a"), b"before\n");
    assert_eq!(fixture.read("b"), b"before\n");
}

#[tokio::test]
async fn write_ahead_manifest_recovers_a_crash_after_writing_without_finalization() {
    let fixture = Fixture::new("write-ahead");
    fixture.write("a", "before\n");
    {
        let mut recording = fixture.journal.lock().await;
        recording
            .prepare(&[(fixture.cwd.join("a"), Some(b"after\n"))])
            .unwrap();
        fixture.write("a", "after\n");
    }
    fixture.resumed().undo().await.unwrap();
    assert_eq!(fixture.read("a"), b"before\n");
}

#[tokio::test]
async fn unexpected_bytes_after_an_interrupted_patch_are_never_blindly_restored() {
    let fixture = Fixture::new("unexpected");
    fixture.write("a", "before\n");
    {
        let mut recording = fixture.journal.lock().await;
        recording
            .prepare(&[(fixture.cwd.join("a"), Some(b"after\n"))])
            .unwrap();
        fixture.write("a", "partial write or user edit");
        assert_eq!(recording.finish().unwrap(), Some(UNAVAILABLE));
        assert_eq!(recording.finish().unwrap(), None);
    }
    assert!(fixture.resumed().undo().await.is_err());
    assert_eq!(fixture.read("a"), b"partial write or user edit");
}

#[tokio::test]
async fn an_interrupted_undo_can_resume_without_requiring_already_restored_files() {
    let fixture = Fixture::new("undo-interrupted");
    fixture.write("a", "before\n");
    fixture.write("b", "before\n");
    fixture.update("a", "before", "after").await;
    fixture.update("b", "before", "after").await;
    let mut turn = fixture.journal.latest().unwrap().unwrap();
    turn.manifest.status = TurnStatus::Undoing;
    turn.save().unwrap();
    fixture.write("a", "before\n");
    fixture.resumed().undo().await.unwrap();
    assert_eq!(fixture.read("a"), b"before\n");
    assert_eq!(fixture.read("b"), b"before\n");
}

#[tokio::test]
async fn journal_failure_blocks_patching_before_writing_any_workspace_file() {
    let fixture = Fixture::new("journal-failure");
    fixture.write("a", "before\n");
    let root = fixture.journal.directory.as_ref().unwrap();
    std::fs::create_dir_all(root.parent().unwrap()).unwrap();
    std::fs::write(root, "not a directory").unwrap();
    assert!(
        fixture
            .patch("*** Update File: a\n@@\n-before\n+after\n")
            .await
            .is_err()
    );
    assert_eq!(fixture.read("a"), b"before\n");
}

#[tokio::test]
async fn invalidation_must_be_persisted_before_further_patch_writes() {
    let fixture = Fixture::new("invalidation-failure");
    fixture.write("a", "original\n");
    fixture.update("a", "original", "agent").await;
    fixture.write("a", "formatted\n");
    let turn = fixture.journal.latest().unwrap().unwrap();
    let manifest = turn.directory.join("manifest.json");
    std::fs::remove_file(&manifest).unwrap();
    std::fs::create_dir_all(&manifest).unwrap();
    let patch = "*** Update File: a\n@@\n-formatted\n+final\n";
    for _ in 0..2 {
        assert!(fixture.patch(patch).await.is_err());
        assert_eq!(fixture.read("a"), b"formatted\n");
    }
    std::fs::remove_dir(&manifest).unwrap();
    turn.save().unwrap();
    let result = fixture.patch(patch).await.unwrap();
    assert!(result_text(&result).contains(UNAVAILABLE));
    assert_eq!(fixture.read("a"), b"final\n");
    assert!(fixture.resumed().undo().await.is_err());
}

#[tokio::test]
async fn disabled_persistence_does_not_prevent_normal_editing_or_create_artifacts() {
    let fixture = Fixture::new("disabled");
    let journal = shared(None, &fixture.cwd);
    ApplyPatchTool::new(&fixture.cwd)
        .with_undo(Arc::clone(&journal))
        .execute(
            "patch",
            serde_json::json!({"input": "*** Begin Patch\n*** Add File: a\n+new\n*** End Patch"}),
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap();
    assert_eq!(fixture.read("a"), b"new\n");
    assert!(!fixture.root.join("home").exists());
    assert!(journal.undo().await.unwrap_err().contains("cupel home"));
}

#[tokio::test]
async fn no_patch_means_no_journal_and_cancellation_does_not_leave_snapshots() {
    let fixture = Fixture::new("lazy");
    fixture.journal.begin_turn().await;
    assert!(
        fixture
            .journal
            .changes()
            .await
            .unwrap()
            .contains("No recorded")
    );
    assert!(!fixture.root.join("home").exists());
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(ApplyPatchTool::new(&fixture.cwd).with_undo(Arc::clone(&fixture.journal))
        .execute("patch", serde_json::json!({"input": "*** Begin Patch\n*** Add File: a\n+new\n*** End Patch"}), cancel, None).await.is_err());
    assert!(!fixture.root.join("home").exists());
}

#[tokio::test]
async fn concurrent_patch_calls_are_one_complete_undoable_turn() {
    let fixture = Fixture::new("parallel");
    let futures = (0..12).map(|index| {
        let fixture = &fixture;
        async move {
            fixture
                .patch(&format!("*** Add File: file-{index}\n+agent\n"))
                .await
                .unwrap();
        }
    });
    futures_util::future::join_all(futures).await;
    assert_eq!(fixture.resumed().undo().await.unwrap().len(), 12);
    for index in 0..12 {
        assert!(!fixture.cwd.join(format!("file-{index}")).exists());
    }
}

#[tokio::test]
async fn cleanup_removes_all_undo_turns_but_preserves_other_session_data() {
    let fixture = Fixture::new("cleanup");
    fixture.write("a", "original\n");
    fixture.update("a", "original", "first").await;
    fixture.journal.begin_turn().await;
    fixture.update("a", "first", "second").await;
    let root = fixture.journal.directory.as_ref().unwrap();
    assert_eq!(turn_directories(root).unwrap().len(), 2);
    std::fs::write(&fixture.transcript, "transcript").unwrap();
    let namespace = root.parent().unwrap();
    let helper = namespace.join("helper.jsonl");
    std::fs::write(&helper, "helper transcript").unwrap();
    let other = fixture
        .transcript
        .parent()
        .unwrap()
        .join("other-session/undo");
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(other.join("keep"), "other snapshots").unwrap();

    fixture.journal.cleanup().await.unwrap();
    assert!(!root.exists());
    assert!(fixture.journal.state.lock().await.active.is_none());
    assert_eq!(fixture.read("a"), b"second\n");
    assert_eq!(std::fs::read(&fixture.transcript).unwrap(), b"transcript");
    assert_eq!(std::fs::read(&helper).unwrap(), b"helper transcript");
    assert_eq!(
        std::fs::read(other.join("keep")).unwrap(),
        b"other snapshots"
    );
    assert!(
        fixture
            .resumed()
            .undo()
            .await
            .unwrap_err()
            .contains("No recorded")
    );
    fixture.journal.cleanup().await.unwrap();
    std::fs::remove_file(helper).unwrap();
    fixture.journal.cleanup().await.unwrap();
    assert!(
        !namespace.exists(),
        "empty per-session directories are removed too"
    );
    assert!(fixture.transcript.exists());
    assert!(other.exists());
}

#[tokio::test]
async fn cleanup_waits_for_inflight_patch_recording() {
    let fixture = Fixture::new("cleanup-lock");
    fixture.write("a", "original\n");
    let mut recording = fixture.journal.lock().await;
    recording
        .prepare(&[(fixture.cwd.join("a"), Some(b"edited\n"))])
        .unwrap();
    let journal = Arc::clone(&fixture.journal);
    let mut cleanup = tokio::spawn(async move { journal.cleanup().await });
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(10), &mut cleanup)
            .await
            .is_err()
    );
    fixture.write("a", "edited\n");
    recording.finish().unwrap();
    drop(recording);
    cleanup.await.unwrap().unwrap();
    assert!(!fixture.journal.directory.as_ref().unwrap().exists());
    assert_eq!(fixture.read("a"), b"edited\n");
}

#[tokio::test]
async fn cleanup_failures_report_the_path_and_can_be_retried() {
    let fixture = Fixture::new("cleanup-failure");
    fixture.write("a", "original\n");
    fixture.update("a", "original", "edited").await;
    let root = fixture.journal.directory.as_ref().unwrap();
    let saved = root.with_extension("saved");
    std::fs::rename(root, &saved).unwrap();
    std::fs::write(root, "not a directory").unwrap();
    let error = fixture.journal.cleanup().await.unwrap_err();
    assert!(error.contains(&root.display().to_string()), "{error}");
    assert!(fixture.journal.state.lock().await.active.is_some());
    std::fs::remove_file(root).unwrap();
    std::fs::rename(saved, root).unwrap();
    fixture.journal.cleanup().await.unwrap();
    assert!(!root.exists());
    assert_eq!(fixture.read("a"), b"edited\n");
}

#[tokio::test]
async fn cleanup_without_persistence_never_creates_directories() {
    let fixture = Fixture::new("cleanup-disabled");
    let journal = shared(None, &fixture.cwd);
    journal.cleanup().await.unwrap();
    assert!(!fixture.root.join("home").exists());
}

#[tokio::test]
async fn ending_a_session_cleans_undo_even_without_a_prompt_hook() {
    let fixture = Fixture::new("cleanup-end-session");
    for started in [false, true] {
        let mut recorder = SessionRecorder::new(
            Some(fixture.root.join("home")),
            &fixture.cwd,
            if started { "started" } else { "unstarted" },
            "model",
        );
        if started {
            recorder.before_prompt("edit a").await;
        }
        recorder.record(&cupel_agent::AgentMessage::user_text("edit a"));
        let transcript = recorder.transcript().unwrap().to_path_buf();
        let undo = transcript.with_extension("").join("undo");
        fixture.write("a", "original\n");
        ApplyPatchTool::new(&fixture.cwd).with_undo(recorder.undo()).execute(
            "patch",
            serde_json::json!({"input": "*** Begin Patch\n*** Update File: a\n@@\n-original\n+edited\n*** End Patch"}),
            CancellationToken::new(), None,
        ).await.unwrap();
        assert!(undo.exists());
        let before = std::fs::read(&transcript).unwrap();
        recorder.end_session().await;
        assert!(!undo.exists());
        assert_eq!(std::fs::read(&transcript).unwrap(), before);
        assert_eq!(fixture.read("a"), b"edited\n");
        assert!(
            recorder
                .undo()
                .undo()
                .await
                .unwrap_err()
                .contains("No recorded")
        );
        recorder.end_session().await;
    }
}

#[cfg(unix)]
#[tokio::test]
async fn private_snapshots_preserve_original_permissions_and_refuse_later_chmod() {
    use std::os::unix::fs::PermissionsExt as _;
    let fixture = Fixture::new("permissions");
    fixture.write("script", "before\n");
    let path = fixture.cwd.join("script");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o751)).unwrap();
    fixture.update("script", "before", "after").await;
    let turn = fixture.journal.latest().unwrap().unwrap();
    for directory in [
        fixture.journal.directory.as_ref().unwrap().clone(),
        turn.directory.clone(),
        turn.directory.join("before"),
    ] {
        assert_eq!(
            std::fs::metadata(directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
    assert_eq!(
        std::fs::metadata(turn.backup(0))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    assert!(fixture.journal.undo().await.is_err());
    assert_eq!(fixture.read("script"), b"after\n");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o751)).unwrap();
    fixture.journal.undo().await.unwrap();
    assert_eq!(
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
        0o751
    );
}

#[cfg(unix)]
#[tokio::test]
async fn symlink_and_hardlink_leaves_refuse_before_other_patch_writes() {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new("links");
    fixture.write("real", "mine\n");
    symlink(fixture.cwd.join("real"), fixture.cwd.join("link")).unwrap();
    assert!(
        fixture
            .patch("*** Add File: other\n+created\n*** Update File: link\n@@\n-mine\n+agent\n")
            .await
            .is_err()
    );
    assert!(!fixture.cwd.join("other").exists());
    assert_eq!(fixture.read("real"), b"mine\n");
    std::fs::hard_link(fixture.cwd.join("real"), fixture.cwd.join("hard")).unwrap();
    assert!(
        fixture
            .patch("*** Update File: hard\n@@\n-mine\n+agent\n")
            .await
            .is_err()
    );
    assert_eq!(fixture.read("real"), b"mine\n");
}

#[cfg(unix)]
#[tokio::test]
async fn retargeted_symlink_parent_and_new_symlink_leaves_block_undo() {
    use std::os::unix::fs::symlink;
    let fixture = Fixture::new("retarget");
    fixture.write("one/a", "mine\n");
    fixture.write("two/a", "agent\n");
    let alias = fixture.cwd.join("alias");
    symlink(fixture.cwd.join("one"), &alias).unwrap();
    fixture.update("alias/a", "mine", "agent").await;
    std::fs::remove_file(&alias).unwrap();
    symlink(fixture.cwd.join("two"), &alias).unwrap();
    assert!(fixture.resumed().undo().await.is_err());
    assert_eq!(fixture.read("one/a"), b"agent\n");
    assert_eq!(fixture.read("two/a"), b"agent\n");
    std::fs::remove_file(&alias).unwrap();
    symlink(fixture.cwd.join("one"), &alias).unwrap();
    std::fs::remove_file(fixture.cwd.join("one/a")).unwrap();
    symlink(fixture.cwd.join("two/a"), fixture.cwd.join("one/a")).unwrap();
    assert!(fixture.resumed().undo().await.is_err());
    assert_eq!(fixture.read("two/a"), b"agent\n");
}

#[tokio::test]
async fn undo_context_notice_is_kept_in_memory_and_in_the_resumed_transcript() {
    let fixture = Fixture::new("notice");
    let home = fixture.root.join("real-home");
    let mut recorder = SessionRecorder::new(Some(home), &fixture.cwd, "session", "model");
    let journal = recorder.undo();
    fixture.write("a", "before\n");
    ApplyPatchTool::new(&fixture.cwd).with_undo(journal).execute("patch", serde_json::json!({"input": "*** Begin Patch\n*** Update File: a\n@@\n-before\n+after\n*** End Patch"}), CancellationToken::new(), None).await.unwrap();
    let model = cupel_core::catalog::builtin_models().remove(0);
    let mut agent = cupel_agent::Agent::new(cupel_agent::AgentOptions::new(
        model,
        Arc::new(cupel_core::provider::Registry::new()),
    ));
    let output = execute_command(UndoCommand::Undo, &mut agent, &mut recorder)
        .await
        .unwrap();
    assert!(output.contains("Undid"));
    assert_eq!(fixture.read("a"), b"before\n");
    let (_, messages) = crate::session::load_transcript(recorder.transcript().unwrap()).unwrap();
    assert_eq!(messages, agent.state().messages);
    assert!(
        matches!(&messages[0], cupel_agent::AgentMessage::Llm(cupel_core::types::Message::User(user)) if matches!(&user.content, cupel_core::types::UserContentBody::Text(text) if text.contains("user ran /undo") && text.contains("Read these files again")))
    );
}

#[tokio::test]
async fn malformed_manifests_and_wrong_working_directories_never_restore_files() {
    let fixture = Fixture::new("manifest");
    fixture.write("a", "before\n");
    fixture.update("a", "before", "after").await;
    let mut turn = fixture.journal.latest().unwrap().unwrap();
    let path = turn.directory.join("manifest.json");
    std::fs::write(&path, "{broken").unwrap();
    assert!(
        fixture
            .resumed()
            .undo()
            .await
            .unwrap_err()
            .contains("invalid undo manifest")
    );
    turn.manifest.cwd = fixture.root.clone();
    turn.save().unwrap();
    assert!(
        fixture
            .resumed()
            .undo()
            .await
            .unwrap_err()
            .contains("working directory")
    );
    turn.manifest.cwd = fixture.cwd.clone();
    turn.manifest.version = 100;
    turn.save().unwrap();
    assert!(
        fixture
            .resumed()
            .undo()
            .await
            .unwrap_err()
            .contains("version")
    );
    assert_eq!(fixture.read("a"), b"after\n");
}

#[tokio::test]
async fn replacement_rechecks_current_state_after_staging_and_cleans_its_temporary_file() {
    let fixture = Fixture::new("replacement-guard");
    fixture.write("a", "before\n");
    fixture.update("a", "before", "after").await;
    let turn = fixture.journal.latest().unwrap().unwrap();
    let entry = &turn.manifest.entries[0];
    fixture.write("a", "newer user edit\n");
    assert!(atomic_write(&entry.path, b"before\n", None, Some(entry)).is_err());
    assert_eq!(fixture.read("a"), b"newer user edit\n");
    assert_eq!(std::fs::read_dir(&fixture.cwd).unwrap().count(), 1);
}

#[tokio::test]
async fn patches_cannot_modify_their_own_snapshot_store() {
    let fixture = Fixture::new("self-journal");
    fixture.patch("*** Add File: a\n+agent\n").await.unwrap();
    let turn = fixture.journal.latest().unwrap().unwrap();
    let error = fixture
        .patch(&format!(
            "*** Add File: {}\n+corrupt\n",
            turn.directory.join("manifest.json").display()
        ))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("own undo journal"));
    fixture.resumed().undo().await.unwrap();
    assert!(!fixture.cwd.join("a").exists());
}

#[tokio::test]
async fn unavailable_tracking_still_protects_its_snapshot_store() {
    let fixture = Fixture::new("unavailable-self-journal");
    fixture.write("a", "original\n");
    fixture.update("a", "original", "agent").await;
    fixture.write("a", "formatted\n");
    fixture.update("a", "formatted", "final").await;
    let turn = fixture.journal.latest().unwrap().unwrap();
    let manifest = turn.directory.join("manifest.json");
    let before = std::fs::read(&manifest).unwrap();
    let error = fixture
        .patch(&format!("*** Add File: {}\n+corrupt\n", manifest.display()))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("own undo journal"));
    assert_eq!(std::fs::read(manifest).unwrap(), before);
    assert!(fixture.resumed().undo().await.is_err());
    assert_eq!(fixture.read("a"), b"final\n");
}

#[tokio::test]
async fn shell_only_files_are_not_included_and_shell_overwrites_block_undo() {
    use crate::tools::bash::BashTool;
    let fixture = Fixture::new("shell");
    fixture.write("a", "mine\n");
    fixture.update("a", "mine", "agent").await;
    BashTool::new(&fixture.cwd)
        .execute(
            "shell",
            serde_json::json!({"command": "printf 'shell' > shell-only"}),
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap();
    fixture.journal.undo().await.unwrap();
    assert_eq!(fixture.read("a"), b"mine\n");
    assert_eq!(fixture.read("shell-only"), b"shell");
    fixture.journal.begin_turn().await;
    fixture.update("a", "mine", "agent").await;
    BashTool::new(&fixture.cwd)
        .execute(
            "shell",
            serde_json::json!({"command": "printf 'shell' > a"}),
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap();
    assert!(fixture.journal.undo().await.is_err());
    assert_eq!(fixture.read("a"), b"shell");
}

#[cfg(unix)]
#[tokio::test]
async fn preview_includes_permission_only_changes_and_net_unchanged_turns_are_skipped() {
    use std::os::unix::fs::PermissionsExt as _;
    let fixture = Fixture::new("mode-only");
    fixture.write("a", "same\n");
    let path = fixture.cwd.join("a");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    fixture.patch("*** Delete File: a\n").await.unwrap();
    fixture.patch("*** Add File: a\n+same\n").await.unwrap();
    assert!(
        fixture
            .journal
            .changes()
            .await
            .unwrap()
            .contains("permissions changed")
    );
    fixture.journal.undo().await.unwrap();
    assert_eq!(
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
        0o755
    );
    fixture.journal.begin_turn().await;
    fixture.update("a", "same", "same").await;
    assert!(
        fixture
            .journal
            .changes()
            .await
            .unwrap()
            .contains("No recorded")
    );
}
