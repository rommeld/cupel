//! The `write` tool. Port of pi's `tools/write.ts`.
//!
//! Deliberately blunt: create-or-overwrite the whole file, making parent
//! directories as needed. The system prompt steers the model toward `edit`
//! for existing files; `write` is for new files and full rewrites, so it
//! carries none of edit's matching machinery. Mutations go through the same
//! per-file queue as `edit` so a parallel batch can't interleave a write
//! with an edit on the same path.

use std::path::PathBuf;

use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use cupel_agent::types::{AgentTool, AgentToolResult, ToolError, ToolUpdateFn};

use super::file_queue::lock_file_for_mutation;
use crate::search::resolve_to_root;

#[derive(Debug, Deserialize)]
struct WriteArgs {
    path: String,
    content: String,
}

pub struct WriteTool {
    cwd: PathBuf,
}

impl WriteTool {
    #[must_use]
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self { cwd: cwd.into() }
    }
}

#[async_trait::async_trait]
impl AgentTool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }

    fn description(&self) -> &str {
        "Write content to a file. Creates the file if it doesn't exist, overwrites if it does. \
         Automatically creates parent directories."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to write (relative or absolute)"
                },
                "content": {
                    "type": "string",
                    "description": "Content to write to the file"
                }
            },
            "required": ["path", "content"]
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        args: Value,
        cancel: CancellationToken,
        _on_update: Option<ToolUpdateFn>,
    ) -> Result<AgentToolResult, ToolError> {
        let args: WriteArgs = serde_json::from_value(args)?;
        let absolute_path = resolve_to_root(&args.path, &self.cwd);

        let _guard = lock_file_for_mutation(&absolute_path).await;
        if cancel.is_cancelled() {
            return Err("Operation aborted".into());
        }

        if let Some(parent) = absolute_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| format!("Could not create directories for {}: {e}", args.path))?;
        }
        if cancel.is_cancelled() {
            return Err("Operation aborted".into());
        }

        tokio::fs::write(&absolute_path, &args.content)
            .await
            .map_err(|e| format!("Could not write file: {}. {e}.", args.path))?;

        Ok(AgentToolResult::text(format!(
            "Successfully wrote {} bytes to {}",
            args.content.len(),
            args.path
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn writes_and_creates_parent_directories() {
        let dir = std::env::temp_dir().join("cupel-write-test");
        // Fresh subtree so the nested-dir assertion is meaningful.
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let tool = WriteTool::new(&dir);
        let result = tool
            .execute(
                "c",
                json!({"path": "deep/nested/file.txt", "content": "hello"}),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();

        let written = std::fs::read_to_string(dir.join("deep/nested/file.txt")).unwrap();
        assert_eq!(written, "hello");
        let cupel_core::types::ToolResultContent::Text(text) = &result.content[0] else {
            panic!("expected text result");
        };
        assert!(text.text.contains("5 bytes"));
    }
}
