//! The `read` tool. Port of pi's `tools/read.ts`.
//!
//! Text files stream back with head truncation (2000 lines / 50 KB) and
//! *actionable* continuation notices - "use offset=N to continue" teaches
//! the model how to page through big files instead of giving up. Images are
//! detected by extension and returned as base64 attachments; the core
//! transform layer already downgrades them to a text placeholder for models
//! without vision, so the tool doesn't need to know the active model.
//!
//! Simplifications vs. pi (documented, revisit if they bite): no automatic
//! image resizing (pi resizes to 2000x2000 via sharp) and no macOS filename
//! fallbacks (NFD normalization, curly-quote screenshot names).

use std::path::PathBuf;

use base64::Engine as _;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use cupel_agent::types::{AgentTool, AgentToolResult, ToolError, ToolUpdateFn};
use cupel_core::types::{ImageContent, TextContent, ToolResultContent};

use crate::search::resolve_to_root;
use crate::truncate::{
    DEFAULT_MAX_BYTES, DEFAULT_MAX_LINES, TruncatedBy, TruncationOptions, format_size,
    truncate_head,
};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ReadArgs {
    path: String,
    /// 1-indexed line to start from.
    offset: Option<usize>,
    /// Maximum number of lines to return.
    limit: Option<usize>,
}

/// Map a file extension to a provider-supported image MIME type.
fn image_mime_type(path: &std::path::Path) -> Option<&'static str> {
    match path
        .extension()
        .and_then(|e| e.to_str())?
        .to_ascii_lowercase()
        .as_str()
    {
        "jpg" | "jpeg" => Some("image/jpeg"),
        "png" => Some("image/png"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        _ => None,
    }
}

pub struct ReadTool {
    cwd: PathBuf,
    description: String,
}

impl ReadTool {
    #[must_use]
    pub fn new(cwd: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            description: format!(
                "Read the contents of a file. Supports text files and images (jpg, png, gif, \
                 webp). Images are sent as attachments. For text files, output is truncated to \
                 {DEFAULT_MAX_LINES} lines or {}KB (whichever is hit first). Use offset/limit \
                 for large files. When you need the full file, continue with offset until \
                 complete.",
                DEFAULT_MAX_BYTES / 1024
            ),
        }
    }
}

#[async_trait::async_trait]
impl AgentTool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Path to the file to read (relative or absolute)"
                },
                "offset": {
                    "type": "number",
                    "description": "Line number to start reading from (1-indexed)"
                },
                "limit": {
                    "type": "number",
                    "description": "Maximum number of lines to read"
                }
            },
            "required": ["path"]
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        args: Value,
        cancel: CancellationToken,
        _on_update: Option<ToolUpdateFn>,
    ) -> Result<AgentToolResult, ToolError> {
        let args: ReadArgs = serde_json::from_value(args)?;
        let absolute_path = resolve_to_root(&args.path, &self.cwd);

        if cancel.is_cancelled() {
            return Err("Operation aborted".into());
        }

        // ---- Images -----------------------------------------------------------
        if let Some(mime_type) = image_mime_type(&absolute_path) {
            let bytes = tokio::fs::read(&absolute_path).await?;
            let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
            return Ok(AgentToolResult {
                content: vec![
                    ToolResultContent::Text(TextContent::plain(format!(
                        "Read image file [{mime_type}]"
                    ))),
                    ToolResultContent::Image(ImageContent {
                        data,
                        mime_type: mime_type.to_string(),
                    }),
                ],
                details: None,
                terminate: false,
            });
        }

        // ---- Text --------------------------------------------------------------
        let content = tokio::fs::read_to_string(&absolute_path)
            .await
            .map_err(|e| format!("Could not read file: {} ({e})", args.path))?;
        if cancel.is_cancelled() {
            return Err("Operation aborted".into());
        }

        let all_lines: Vec<&str> = content.split('\n').collect();
        let total_file_lines = all_lines.len();

        // Convert 1-indexed offset to a 0-indexed start.
        let start_line = args.offset.map_or(0, |o| o.saturating_sub(1));
        let start_line_display = start_line + 1;
        if start_line >= all_lines.len() {
            return Err(format!(
                "Offset {} is beyond end of file ({total_file_lines} lines total)",
                args.offset.unwrap_or(0)
            )
            .into());
        }

        // A user-provided limit is honored first; truncate_head still applies
        // its own line/byte caps afterwards.
        let (selected, user_limited_lines) = match args.limit {
            Some(limit) => {
                let end = (start_line + limit).min(all_lines.len());
                (
                    all_lines[start_line..end].join("\n"),
                    Some(end - start_line),
                )
            }
            None => (all_lines[start_line..].join("\n"), None),
        };

        let truncation = truncate_head(&selected, TruncationOptions::default());
        let output = if truncation.first_line_exceeds_limit {
            // Nothing could be kept; point the model at a bash fallback.
            let first_line_size = format_size(all_lines[start_line].len());
            format!(
                "[Line {start_line_display} is {first_line_size}, exceeds {} limit. Use bash: \
                 sed -n '{start_line_display}p' {} | head -c {DEFAULT_MAX_BYTES}]",
                format_size(DEFAULT_MAX_BYTES),
                args.path
            )
        } else if truncation.truncated {
            let end_line_display = start_line_display + truncation.output_lines - 1;
            let next_offset = end_line_display + 1;
            let reason = if truncation.truncated_by == Some(TruncatedBy::Lines) {
                format!(
                    "Showing lines {start_line_display}-{end_line_display} of {total_file_lines}."
                )
            } else {
                format!(
                    "Showing lines {start_line_display}-{end_line_display} of \
                     {total_file_lines} ({} limit).",
                    format_size(DEFAULT_MAX_BYTES)
                )
            };
            format!(
                "{}\n\n[{reason} Use offset={next_offset} to continue.]",
                truncation.content
            )
        } else if let Some(shown) =
            user_limited_lines.filter(|shown| start_line + shown < all_lines.len())
        {
            // The user's limit stopped early but the file continues.
            let remaining = all_lines.len() - (start_line + shown);
            let next_offset = start_line + shown + 1;
            format!(
                "{}\n\n[{remaining} more lines in file. Use offset={next_offset} to continue.]",
                truncation.content
            )
        } else {
            truncation.content
        };

        Ok(AgentToolResult::text(output))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn run_read(dir: &std::path::Path, args: Value) -> Result<String, String> {
        let tool = ReadTool::new(dir);
        let result = tool
            .execute("call_1", args, CancellationToken::new(), None)
            .await
            .map_err(|e| e.to_string())?;
        let ToolResultContent::Text(text) = &result.content[0] else {
            return Err("expected text".to_string());
        };
        Ok(text.text.clone())
    }

    #[tokio::test]
    async fn reads_whole_file() {
        let dir = std::env::temp_dir().join("cupel-read-test-1");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("f.txt"), "one\ntwo\nthree").unwrap();
        let out = run_read(&dir, json!({"path": "f.txt"})).await.unwrap();
        assert_eq!(out, "one\ntwo\nthree");
    }

    #[tokio::test]
    async fn offset_and_limit_add_continuation_notice() {
        let dir = std::env::temp_dir().join("cupel-read-test-2");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("f.txt"), "1\n2\n3\n4\n5").unwrap();
        let out = run_read(&dir, json!({"path": "f.txt", "offset": 2, "limit": 2}))
            .await
            .unwrap();
        assert!(out.starts_with("2\n3"), "got: {out}");
        assert!(out.contains("Use offset=4 to continue"), "got: {out}");
    }

    #[tokio::test]
    async fn offset_beyond_eof_is_an_error() {
        let dir = std::env::temp_dir().join("cupel-read-test-3");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("f.txt"), "only line").unwrap();
        let err = run_read(&dir, json!({"path": "f.txt", "offset": 10}))
            .await
            .unwrap_err();
        assert!(err.contains("beyond end of file"), "got: {err}");
    }

    #[tokio::test]
    async fn image_is_returned_as_attachment() {
        let dir = std::env::temp_dir().join("cupel-read-test-4");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("p.png"), [0x89, 0x50, 0x4E, 0x47]).unwrap();
        let tool = ReadTool::new(&dir);
        let result = tool
            .execute(
                "c",
                json!({"path": "p.png"}),
                CancellationToken::new(),
                None,
            )
            .await
            .unwrap();
        assert_eq!(result.content.len(), 2);
        assert!(
            matches!(&result.content[1], ToolResultContent::Image(img) if img.mime_type == "image/png")
        );
    }
}
