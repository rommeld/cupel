//! One call carries one file path and one or more `{oldText, newText}`
//! replacements. All matching subtleties (fuzzy matching, uniqueness,
//! overlap detection, BOM/CRLF round-tripping) live in
//! [`super::edit_diff`]; this module is the I/O shell around it, serialized
//! per file through the mutation queue so parallel tool batches can't race
//! on the same file.

use std::path::PathBuf;

use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use cupel_agent::types::{AgentTool, AgentToolResult, ToolError, ToolUpdateFn};
use cupel_core::types::{TextContent, ToolResultContent};

use super::edit_diff::{
    Edit, apply_edits, detect_line_ending, generate_diff_string, generate_unified_patch,
    normalize_to_lf, restore_line_endings, strip_bom,
};
use super::file_queue::lock_file_for_mutation;
use crate::search::resolve_to_root;

const DIFF_CONTEXT_LINES: usize = 4;

#[derive(Debug, Deserialize)]
struct EditArgs {
    path: String,
    edits: Vec<Edit>,
    // pi also accepts a legacy single {oldText, newText} pair at the top
    // level; `prepare_arguments` folds that shape into `edits` before this
    // struct ever sees it.
}

pub struct EditTool {
    cwd: PathBuf,
}

impl EditTool {
    #[must_use]
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self { cwd: cwd.into() }
    }

    /// Compatibility shim for raw arguments (pi's `prepareArguments`):
    /// - `edits` sent as a JSON *string* (Opus 4.6, GLM 5.1 do this),
    /// - legacy top-level `oldText`/`newText` folded into `edits`.
    fn prepare_arguments(mut args: Value) -> Value {
        let Some(map) = args.as_object_mut() else {
            return args;
        };
        if let Some(Value::String(edits_json)) = map.get("edits")
            && let Ok(parsed @ Value::Array(_)) = serde_json::from_str::<Value>(edits_json)
        {
            map.insert("edits".to_string(), parsed);
        }
        if let (Some(Value::String(old_text)), Some(Value::String(new_text))) =
            (map.get("oldText").cloned(), map.get("newText").cloned())
        {
            let edits = map
                .entry("edits")
                .or_insert_with(|| Value::Array(Vec::new()));
            if let Value::Array(list) = edits {
                list.push(json!({"oldText": old_text, "newText": new_text}));
            }
            map.remove("oldText");
            map.remove("newText");
        }
        args
    }
}

#[async_trait::async_trait]
impl AgentTool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }

    fn description(&self) -> &str {
        "Edit a single file using exact text replacement. Every edits[].oldText must match a \
         unique, non-overlapping region of the original file. If two changes affect the same \
         block or nearby lines, merge them into one edit instead of emitting overlapping edits. \
         Do not include large unchanged regions just to connect distant changes."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to edit (relative or absolute)"
                },
                "edits": {
                    "type": "array",
                    "description": "One or more targeted replacements. Each edit is matched \
                        against the original file, not incrementally. Do not include \
                        overlapping or nested edits. If two changes touch the same block or \
                        nearby lines, merge them into one edit instead.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "oldText": {
                                "type": "string",
                                "description": "Exact text for one targeted replacement. It \
                                    must be unique in the original file and must not overlap \
                                    with any other edits[].oldText in the same call."
                            },
                            "newText": {
                                "type": "string",
                                "description": "Replacement text for this targeted edit."
                            }
                        },
                        "required": ["oldText", "newText"]
                    }
                }
            },
            "required": ["path", "edits"]
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        args: Value,
        cancel: CancellationToken,
        _on_update: Option<ToolUpdateFn>,
    ) -> Result<AgentToolResult, ToolError> {
        let args: EditArgs = serde_json::from_value(Self::prepare_arguments(args))?;
        if args.edits.is_empty() {
            return Err(
                "Edit tool input is invalid. edits must contain at least one replacement.".into(),
            );
        }
        let absolute_path = resolve_to_root(&args.path, &self.cwd);

        // Hold the per-file lock across the whole read-modify-write cycle.
        // Cancellation is observed BETWEEN operations (not by interrupting
        // them) so the lock always outlives the in-flight filesystem call -
        // same reasoning as pi's throwIfAborted comment.
        let _guard = lock_file_for_mutation(&absolute_path).await;
        if cancel.is_cancelled() {
            return Err("Operation aborted".into());
        }

        let raw_content = tokio::fs::read_to_string(&absolute_path)
            .await
            .map_err(|e| format!("Could not edit file: {}. {e}.", args.path))?;
        if cancel.is_cancelled() {
            return Err("Operation aborted".into());
        }

        // Match in BOM-free, LF-normalized space; restore both on write.
        let (bom, text) = strip_bom(&raw_content);
        let original_ending = detect_line_ending(text);
        let normalized = normalize_to_lf(text);
        let applied = apply_edits(&normalized, &args.edits, &args.path)?;
        if cancel.is_cancelled() {
            return Err("Operation aborted".into());
        }

        let final_content = format!(
            "{bom}{}",
            restore_line_endings(&applied.new_content, original_ending)
        );
        tokio::fs::write(&absolute_path, &final_content)
            .await
            .map_err(|e| format!("Could not write file: {}. {e}.", args.path))?;

        let diff = generate_diff_string(
            &applied.base_content,
            &applied.new_content,
            DIFF_CONTEXT_LINES,
        );
        let patch = generate_unified_patch(
            &args.path,
            &applied.base_content,
            &applied.new_content,
            DIFF_CONTEXT_LINES,
        );

        Ok(AgentToolResult {
            content: vec![ToolResultContent::Text(TextContent::plain(format!(
                "Successfully replaced {} block(s) in {}.",
                args.edits.len(),
                args.path
            )))],
            // The diff/patch ride in details for UIs and logs; the model only
            // needs the confirmation above (it already knows what it changed).
            details: Some(json!({
                "diff": diff.diff,
                "patch": patch,
                "firstChangedLine": diff.first_changed_line,
            })),
            terminate: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(name: &str, content: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join("cupel-edit-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, content).unwrap();
        (dir, path)
    }

    #[tokio::test]
    async fn edits_a_file_and_reports_a_diff() {
        let (dir, path) = temp_file("basic.rs", "fn main() {\n    old();\n}\n");
        let tool = EditTool::new(&dir);
        let result = tool
            .execute(
                "c",
                json!({"path": "basic.rs", "edits": [{"oldText": "old()", "newText": "new()"}]}),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "fn main() {\n    new();\n}\n"
        );
        let details = result.details.unwrap();
        assert!(details["diff"].as_str().unwrap().contains("+2     new();"));
        assert!(details["patch"].as_str().unwrap().contains("@@"));
    }

    #[tokio::test]
    async fn crlf_files_keep_their_line_endings() {
        let (dir, path) = temp_file("crlf.txt", "a\r\nb\r\n");
        let tool = EditTool::new(&dir);
        tool.execute(
            "c",
            json!({"path": "crlf.txt", "edits": [{"oldText": "a", "newText": "x"}]}),
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "x\r\nb\r\n");
    }

    #[tokio::test]
    async fn legacy_top_level_old_new_text_is_accepted() {
        let (dir, path) = temp_file("legacy.txt", "hello world\n");
        let tool = EditTool::new(&dir);
        tool.execute(
            "c",
            json!({"path": "legacy.txt", "oldText": "world", "newText": "cupel"}),
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello cupel\n");
    }

    #[tokio::test]
    async fn edits_sent_as_json_string_are_parsed() {
        let (dir, path) = temp_file("stringly.txt", "alpha\n");
        let tool = EditTool::new(&dir);
        tool.execute(
            "c",
            json!({
                "path": "stringly.txt",
                "edits": "[{\"oldText\": \"alpha\", \"newText\": \"beta\"}]"
            }),
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "beta\n");
    }

    #[tokio::test]
    async fn missing_file_is_a_clear_error() {
        let dir = std::env::temp_dir().join("cupel-edit-test");
        std::fs::create_dir_all(&dir).unwrap();
        let tool = EditTool::new(&dir);
        let err = tool
            .execute(
                "c",
                json!({"path": "no-such-file.txt", "edits": [{"oldText": "a", "newText": "b"}]}),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Could not edit file"));
    }
}
