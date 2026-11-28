//! Offline tool-to-tool source identity and patch safety regressions.
#![allow(clippy::tests_outside_test_module)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use cupel_agent::types::{AgentTool, AgentToolResult, ToolError};
use cupel_coding_agent::bootstrap;
use cupel_coding_agent::source_observations::SourceObservations;
use cupel_coding_agent::tools::apply_patch::ApplyPatchTool;
use cupel_coding_agent::tools::read::ReadTool;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

struct Project(PathBuf);

impl Project {
    fn new(name: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "cupel-source-handoffs-{name}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(path.canonicalize().unwrap())
    }

    fn write(&self, path: &str, text: &str) {
        std::fs::write(self.0.join(path), text).unwrap();
    }

    fn read(&self, path: &str) -> String {
        std::fs::read_to_string(self.0.join(path)).unwrap()
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn tool<'tools>(tools: &'tools [Arc<dyn AgentTool>], name: &str) -> &'tools dyn AgentTool {
    tools
        .iter()
        .find(|tool| tool.name() == name)
        .unwrap()
        .as_ref()
}

async fn execute(tool: &dyn AgentTool, args: Value) -> Result<AgentToolResult, ToolError> {
    tool.execute("handoff", args, CancellationToken::new(), None)
        .await
}

async fn observe(reader: &dyn AgentTool, path: &str) -> String {
    let read = execute(reader, json!({"path": path, "offset": 2, "limit": 1}))
        .await
        .unwrap();
    read.details.unwrap()["observation"]
        .as_str()
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn fuzzy_patch_preserves_target_context_and_shifts_only_additions() {
    let project = Project::new("fuzzy-indent");
    let patcher = ApplyPatchTool::new(&project.0);
    for (original, context, expected) in [
        (
            "if True:\n    if True:\n        before()  \n        old()\n",
            "     before()",
            "if True:\n    if True:\n        before()  \n        if ready():\n            new()\n",
        ),
        (
            "if True:\n    if True:\n        before()  # smart \u{2019}\n        old()\n",
            "     before()  # smart '",
            "if True:\n    if True:\n        before()  # smart \u{2019}\n        if ready():\n            new()\n",
        ),
    ] {
        project.write("f.py", original);
        let patch = format!(
            "*** Begin Patch\n*** Update File: f.py\n@@\n{context}\n-    old()\n+    if ready():\n+        new()\n*** End Patch"
        );
        execute(&patcher, json!({"input": patch})).await.unwrap();
        assert_eq!(project.read("f.py"), expected);
    }
}

#[tokio::test]
async fn uncertain_fuzzy_indentation_rejects_the_patch_before_any_writes() {
    let project = Project::new("uncertain-indent");
    let patcher = ApplyPatchTool::new(&project.0);
    let patch = "*** Begin Patch\n*** Add File: created.txt\n+created\n*** Update File: f.py\n@@\n     before()\n-    old()\n+    new()\n*** End Patch";
    for original in [
        "        before()\n    old()\n",
        "\t    before()\n\t    old()\n",
    ] {
        project.write("f.py", original);
        let result = execute(&patcher, json!({"input": patch})).await;
        assert_eq!(project.read("f.py"), original);
        assert!(!project.0.join("created.txt").exists());
        let error = result.unwrap_err().to_string();
        assert!(
            error.contains("indentation") && error.contains("f.py"),
            "{error}"
        );
    }
}

#[tokio::test]
async fn unknown_arguments_are_rejected_before_tools_run() {
    let project = Project::new("unknown-arguments");
    let tools = bootstrap::coding_tools(&project.0);
    project.write("file", "old\nunrelated line\n");
    let id = observe(tool(&tools, "read"), "file").await;
    let current = "old\nnew unrelated line\n";
    project.write("file", current);
    let patch = "*** Begin Patch\n*** Update File: file\n@@\n-old\n+new\n*** End Patch";
    let cases = [
        (
            "apply_patch",
            json!({"input": patch, "expected_observations": [id]}),
            "expected_observations",
            "expectedObservations",
        ),
        (
            "bash",
            json!({"command": "touch ran-bash", "cwd": "."}),
            "cwd",
            "command",
        ),
        (
            "read",
            json!({"path": "file", "startLine": 999}),
            "startLine",
            "offset",
        ),
        (
            "grep",
            json!({"pattern": "old", "ignore_case": true}),
            "ignore_case",
            "ignoreCase",
        ),
    ];
    for (name, args, unknown, valid) in cases {
        let result = execute(tool(&tools, name), args).await;
        assert_eq!(project.read("file"), current);
        assert!(!project.0.join("ran-bash").exists());
        let error = result.expect_err(name).to_string();
        assert!(
            error.contains(&format!("unknown field `{unknown}`")),
            "{name}: {error}"
        );
        assert!(error.contains(&format!("`{valid}`")), "{name}: {error}");
    }
}

#[tokio::test]
async fn whole_file_preconditions_reject_unrelated_edits_before_any_writes() {
    let project = Project::new("stale");
    let tools = bootstrap::coding_tools(&project.0);
    for path in ["source.py", "source.ts", "source.java"] {
        project.write(path, "before\nold\nafter\n");
        let id = observe(tool(&tools, "read"), path).await;
        // The inspected target still matches. Only an unrelated line changed.
        project.write(path, "before\nold\nafter formatted\n");
        let patch = format!(
            "*** Begin Patch\n*** Add File: created.txt\n+created\n*** Update File: {path}\n@@\n-old\n+new\n*** End Patch"
        );
        let error = execute(
            tool(&tools, "apply_patch"),
            json!({"input": patch, "expectedObservations": [id]}),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("Stale source observation"), "{error}");
        assert_eq!(project.read(path), "before\nold\nafter formatted\n");
        assert!(!project.0.join("created.txt").exists());
        let fresh = observe(tool(&tools, "read"), path).await;
        execute(
            tool(&tools, "apply_patch"),
            json!({"input": patch, "expectedObservations": [fresh]}),
        )
        .await
        .unwrap();
        assert_eq!(project.read(path), "before\nnew\nafter formatted\n");
        std::fs::remove_file(project.0.join("created.txt")).unwrap();
    }
}

#[tokio::test]
async fn raw_byte_preconditions_survive_normalization_but_detect_format_changes() {
    let project = Project::new("raw-bytes");
    let tools = bootstrap::coding_tools(&project.0);
    let raw = "\u{feff}before\r\nold\r\nafter\r\n";
    project.write("file", raw);
    let id = observe(tool(&tools, "read"), "file").await;
    let patch = "*** Begin Patch\n*** Update File: ./file\n@@\n-old\n+new\n*** End Patch";
    let patcher = tool(&tools, "apply_patch");
    let result = execute(
        patcher,
        json!({"input": patch, "expectedObservations": [id]}),
    )
    .await
    .unwrap();
    assert_eq!(project.read("file"), "\u{feff}before\r\nnew\r\nafter\r\n");
    let details = result.details.unwrap();
    let written = details["observations"][0]["observation"].as_str().unwrap();
    let cupel_core::types::ToolResultContent::Text(text) = &result.content[0] else {
        panic!("expected a patch summary");
    };
    assert!(text.text.contains(written), "{}", text.text);
    assert_eq!(details["observations"][0]["path"], "file");
    let diff = details["diff"].as_str().unwrap();
    assert!(diff.lines().any(|line| line == "+2 new"), "{diff}");

    let second_patch = "*** Begin Patch\n*** Update File: file\n@@\n-new\n+next\n*** End Patch";
    let second = execute(
        patcher,
        json!({"input": second_patch, "expectedObservations": [written]}),
    )
    .await
    .unwrap();
    let updated = "\u{feff}before\r\nnext\r\nafter\r\n";
    assert_eq!(project.read("file"), updated);
    let normalized = updated.replace("\r\n", "\n");
    project.write("file", &normalized);
    let third_patch = "*** Begin Patch\n*** Update File: file\n@@\n-next\n+last\n*** End Patch";
    let second_details = second.details.unwrap();
    let written = &second_details["observations"][0]["observation"];
    let error = execute(
        patcher,
        json!({"input": third_patch, "expectedObservations": [written]}),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(error.contains("Stale source observation"), "{error}");
    assert_eq!(project.read("file"), normalized);

    project.write("file", raw);
    let id = observe(tool(&tools, "read"), "file").await;
    std::fs::write(project.0.join("file"), b"before\nold\n\xff").unwrap();
    let error = execute(
        tool(&tools, "apply_patch"),
        json!({"input": patch, "expectedObservations": [id.clone()]}),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(error.contains("Stale source observation"), "{error}");
    std::fs::remove_file(project.0.join("file")).unwrap();
    let error = execute(
        tool(&tools, "apply_patch"),
        json!({"input": patch, "expectedObservations": [id]}),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("Observed source") && error.contains("unavailable"),
        "{error}"
    );
}

#[tokio::test]
async fn preconditions_also_protect_deletes_overwrites_and_move_destinations() {
    for (name, patch) in [
        ("delete", "*** Delete File: protected"),
        ("overwrite", "*** Add File: protected\n+replacement"),
        (
            "move-destination",
            "*** Update File: source\n*** Move to: protected\n@@\n-old\n+new",
        ),
    ] {
        let project = Project::new(name);
        let tools = bootstrap::coding_tools(&project.0);
        project.write("protected", "before\nobserved\n");
        project.write("source", "old\n");
        let id = observe(tool(&tools, "read"), "protected").await;
        project.write("protected", "before\nexternally changed\n");
        let input = format!("*** Begin Patch\n{patch}\n*** End Patch");
        let error = execute(
            tool(&tools, "apply_patch"),
            json!({"input": input, "expectedObservations": [id]}),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(
            error.contains("Stale source observation"),
            "{name}: {error}"
        );
        assert_eq!(project.read("protected"), "before\nexternally changed\n");
        assert_eq!(project.read("source"), "old\n");
        let fresh = observe(tool(&tools, "read"), "protected").await;
        let result = execute(
            tool(&tools, "apply_patch"),
            json!({"input": input, "expectedObservations": [fresh]}),
        )
        .await
        .unwrap();
        let details = result.details.unwrap();
        let observations = details["observations"].as_array().unwrap();
        match name {
            "delete" => {
                assert!(!project.0.join("protected").exists());
                assert!(observations.is_empty());
            }
            "overwrite" => assert_eq!(project.read("protected"), "replacement\n"),
            _ => {
                assert_eq!(project.read("protected"), "new\n");
                assert!(!project.0.join("source").exists());
            }
        }
        if name != "delete" {
            assert_eq!(observations.len(), 1);
            assert_eq!(observations[0]["path"], "protected");
            let follow_up = "*** Begin Patch\n*** Add File: protected\n+follow-up\n*** End Patch";
            execute(
                tool(&tools, "apply_patch"),
                json!({"input": follow_up, "expectedObservations": [observations[0]["observation"]]}),
            )
            .await
            .unwrap();
            assert_eq!(project.read("protected"), "follow-up\n");
        }
    }
}

#[tokio::test]
async fn unknown_unrelated_duplicate_and_wrong_checkout_observations_fail_closed() {
    let project = Project::new("bad-references");
    let other = Project::new("other-checkout");
    project.write("file", "before\nold\n");
    project.write("unrelated", "before\nold\n");
    other.write("file", "before\nold\n");
    let store = Arc::new(SourceObservations::new());
    let reader = ReadTool::new(&project.0).with_observations(Arc::clone(&store));
    let patcher = ApplyPatchTool::new(&project.0).with_observations(Arc::clone(&store));
    let id = observe(&reader, "file").await;
    let unrelated = observe(&reader, "unrelated").await;
    let input = "*** Begin Patch\n*** Update File: file\n@@\n-old\n+new\n*** End Patch";
    for (ids, expected) in [
        (vec!["unknown".to_string()], "unavailable or expired"),
        (vec![unrelated], "does not identify a current patch target"),
        (vec![id.clone(), id.clone()], "Multiple source observations"),
    ] {
        let error = execute(
            &patcher,
            json!({"input": input, "expectedObservations": ids}),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains(expected), "{error}");
        assert_eq!(project.read("file"), "before\nold\n");
    }
    let wrong_checkout = ApplyPatchTool::new(&other.0).with_observations(store);
    let error = execute(
        &wrong_checkout,
        json!({"input": input, "expectedObservations": [id.clone()]}),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(error.contains("different checkout"), "{error}");
    let unbound = ApplyPatchTool::new(&project.0);
    assert!(
        execute(
            &unbound,
            json!({"input": input, "expectedObservations": [id]})
        )
        .await
        .is_err()
    );
    // Ordinary path-based patches remain available without observation state.
    execute(&unbound, json!({"input": input})).await.unwrap();
    assert_eq!(project.read("file"), "before\nnew\n");
}

#[cfg(unix)]
#[tokio::test]
async fn an_alias_can_use_the_observation_but_retargeting_it_cannot() {
    use std::os::unix::fs::symlink;

    let project = Project::new("symlink");
    let tools = bootstrap::coding_tools(&project.0);
    project.write("first", "before\nold\n");
    project.write("second", "before\nold\n");
    let alias = project.0.join("alias");
    symlink(Path::new("first"), &alias).unwrap();
    let id = observe(tool(&tools, "read"), "alias").await;
    let input = "*** Begin Patch\n*** Update File: alias\n@@\n-old\n+new\n*** End Patch";
    std::fs::remove_file(&alias).unwrap();
    symlink(Path::new("second"), &alias).unwrap();
    let error = execute(
        tool(&tools, "apply_patch"),
        json!({"input": input, "expectedObservations": [id.clone()]}),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("does not identify a current patch target"),
        "{error}"
    );
    assert_eq!(project.read("first"), "before\nold\n");
    assert_eq!(project.read("second"), "before\nold\n");
    std::fs::remove_file(&alias).unwrap();
    symlink(Path::new("first"), &alias).unwrap();
    execute(
        tool(&tools, "apply_patch"),
        json!({"input": input, "expectedObservations": [id]}),
    )
    .await
    .unwrap();
    assert_eq!(project.read("first"), "before\nnew\n");
    assert_eq!(project.read("second"), "before\nold\n");
}

#[tokio::test]
async fn new_destination_aliases_reject_before_any_writes() {
    let project = Project::new("new-destination-aliases");
    std::fs::create_dir_all(project.0.join("nested")).unwrap();
    let tools = bootstrap::coding_tools(&project.0);
    let first = "*** Add File: destination.txt\n+first\n";
    for second in [
        "*** Add File: nested/../destination.txt\n+second\n",
        "*** Update File: source.txt\n*** Move to: nested/../destination.txt\n@@\n-old\n+second\n",
    ] {
        for sections in [format!("{first}{second}"), format!("{second}{first}")] {
            project.write("source.txt", "old\n");
            let input = format!(
                "*** Begin Patch\n*** Add File: created.txt\n+created\n{sections}*** End Patch"
            );
            let result = execute(tool(&tools, "apply_patch"), json!({"input": input})).await;
            assert!(
                result.is_err(),
                "alias collision was accepted: {result:?}; destination bytes: {:?}",
                std::fs::read(project.0.join("destination.txt"))
            );
            let error = result.unwrap_err().to_string();
            assert!(error.starts_with("multiple operations target "), "{error}");
            assert_eq!(project.read("source.txt"), "old\n");
            assert!(!project.0.join("destination.txt").exists());
            assert!(!project.0.join("created.txt").exists());
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn new_destination_keys_follow_parent_symlinks_before_dot_dot() {
    use std::os::unix::fs::symlink;

    let project = Project::new("new-destination-symlinks");
    std::fs::create_dir_all(project.0.join("actual/deep")).unwrap();
    symlink(Path::new("actual/deep"), project.0.join("link")).unwrap();
    let tools = bootstrap::coding_tools(&project.0);
    for (destination, alias) in [
        ("actual/destination.txt", "link/../destination.txt"),
        ("actual/deep/destination.txt", "link/destination.txt"),
    ] {
        let input = format!(
            "*** Begin Patch\n*** Add File: created.txt\n+created\n*** Add File: {destination}\n+first\n*** Add File: {alias}\n+second\n*** End Patch"
        );
        let error = execute(tool(&tools, "apply_patch"), json!({"input": input}))
            .await
            .unwrap_err()
            .to_string();
        assert!(error.starts_with("multiple operations target "), "{error}");
        assert!(!project.0.join(destination).exists());
        assert!(!project.0.join("created.txt").exists());
    }
    // Lexically collapsing link/.. would incorrectly merge these distinct files.
    execute(
        tool(&tools, "apply_patch"),
        json!({"input": "*** Begin Patch\n*** Add File: destination.txt\n+top\n*** Add File: link/../destination.txt\n+nested\n*** End Patch"}),
    )
    .await
    .unwrap();
    assert_eq!(project.read("destination.txt"), "top\n");
    assert_eq!(project.read("actual/destination.txt"), "nested\n");
}
