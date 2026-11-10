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
    execute(
        tool(&tools, "apply_patch"),
        json!({"input": patch, "expectedObservations": [id]}),
    )
    .await
    .unwrap();
    assert_eq!(project.read("file"), "\u{feff}before\r\nnew\r\nafter\r\n");
    project.write("file", raw);
    let id = observe(tool(&tools, "read"), "file").await;
    project.write("file", &raw.replace("\r\n", "\n"));
    let error = execute(
        tool(&tools, "apply_patch"),
        json!({"input": patch, "expectedObservations": [id]}),
    )
    .await
    .unwrap_err()
    .to_string();
    assert!(error.contains("Stale source observation"), "{error}");

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
        execute(
            tool(&tools, "apply_patch"),
            json!({"input": input, "expectedObservations": [fresh]}),
        )
        .await
        .unwrap();
        match name {
            "delete" => assert!(!project.0.join("protected").exists()),
            "overwrite" => assert_eq!(project.read("protected"), "replacement\n"),
            _ => {
                assert_eq!(project.read("protected"), "new\n");
                assert!(!project.0.join("source").exists());
            }
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
