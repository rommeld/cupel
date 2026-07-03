//! The `grep` tool. Port of pi's `coding-agent/src/core/tools/grep.ts`.
//!
//! Split of responsibilities:
//! - [`crate::search`] finds matching lines (the pluggable backend);
//! - this module is the model-facing layer: argument schema, context lines,
//!   line/byte truncation, and the output format the model sees
//!   (`path:line: text` for matches, `path-line- text` for context).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use cupel_agent::types::{AgentTool, AgentToolResult, ToolError, ToolUpdateFn};

use crate::search::{CodeSearch, SearchQuery, resolve_to_root};
use crate::truncate::{
    DEFAULT_MAX_BYTES, GREP_MAX_LINE_LENGTH, TruncationOptions, format_size, truncate_head,
    truncate_line,
};

const DEFAULT_LIMIT: usize = 100;

/// Tool arguments. Deserializing into this struct IS the argument
/// validation (unknown fields are ignored, wrong types are errors).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GrepArgs {
    pattern: String,
    path: Option<String>,
    glob: Option<String>,
    #[serde(default)]
    ignore_case: bool,
    #[serde(default)]
    literal: bool,
    /// Lines of context before/after each match.
    #[serde(default)]
    context: u64,
    limit: Option<usize>,
}

pub struct GrepTool {
    /// The agent's working directory; relative paths resolve against it.
    cwd: PathBuf,
    /// Pluggable search backend (grep today, index in iteration two).
    backend: Arc<dyn CodeSearch>,
    description: String,
}

impl GrepTool {
    #[must_use]
    pub fn new(cwd: impl Into<PathBuf>, backend: Arc<dyn CodeSearch>) -> Self {
        Self {
            cwd: cwd.into(),
            backend,
            description: format!(
                "Search file contents for a pattern. Returns matching lines with file paths \
                 and line numbers. Respects .gitignore. Output is truncated to {DEFAULT_LIMIT} \
                 matches or {}KB (whichever is hit first). Long lines are truncated to \
                 {GREP_MAX_LINE_LENGTH} chars.",
                DEFAULT_MAX_BYTES / 1024
            ),
        }
    }
}

#[async_trait::async_trait]
impl AgentTool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters(&self) -> Value {
        // Kept in sync with `GrepArgs` by hand. pi generates this from the
        // TypeBox schema; a Rust equivalent would be the `schemars` derive -
        // a nice later refinement, but one more macro to learn today.
        json!({
            "type": "object",
            "properties": {
                "pattern": {
                    "type": "string",
                    "description": "Search pattern (regex or literal string)"
                },
                "path": {
                    "type": "string",
                    "description": "Directory or file to search (default: current directory)"
                },
                "glob": {
                    "type": "string",
                    "description": "Filter files by glob pattern, e.g. '*.rs' or '**/*.spec.ts'"
                },
                "ignoreCase": {
                    "type": "boolean",
                    "description": "Case-insensitive search (default: false)"
                },
                "literal": {
                    "type": "boolean",
                    "description": "Treat pattern as literal string instead of regex (default: false)"
                },
                "context": {
                    "type": "number",
                    "description": "Number of lines to show before and after each match (default: 0)"
                },
                "limit": {
                    "type": "number",
                    "description": "Maximum number of matches to return (default: 100)"
                }
            },
            "required": ["pattern"]
        })
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        args: Value,
        cancel: CancellationToken,
        _on_update: Option<ToolUpdateFn>,
    ) -> Result<AgentToolResult, ToolError> {
        let args: GrepArgs = serde_json::from_value(args)?;
        let effective_limit = args.limit.unwrap_or(DEFAULT_LIMIT).max(1);

        // Where are we searching? Needed for both the backend query and for
        // making result paths relative + readable.
        let search_path = resolve_to_root(args.path.as_deref().unwrap_or("."), &self.cwd);
        let searching_directory = search_path.is_dir();

        let outcome = self
            .backend
            .search(
                SearchQuery {
                    pattern: args.pattern,
                    path: args.path,
                    glob: args.glob,
                    ignore_case: args.ignore_case,
                    literal: args.literal,
                    limit: effective_limit,
                },
                cancel,
            )
            .await?;

        if outcome.matches.is_empty() {
            return Ok(AgentToolResult::text("No matches found"));
        }

        // ---- Format matches ---------------------------------------------
        let format_path = |path: &Path| -> String {
            if searching_directory
                && let Ok(relative) = path.strip_prefix(&search_path)
                && !relative.as_os_str().is_empty()
            {
                return relative.display().to_string().replace('\\', "/");
            }
            path.file_name().map_or_else(
                || path.display().to_string(),
                |n| n.to_string_lossy().into(),
            )
        };

        // File cache so N matches in one file read it once (context mode).
        let mut file_cache: HashMap<PathBuf, Vec<String>> = HashMap::new();
        let mut output_lines: Vec<String> = Vec::new();
        let mut lines_truncated = false;

        for m in &outcome.matches {
            let relative_path = format_path(&m.path);

            if args.context == 0 {
                // Fast path: the backend already gave us the matching line.
                let (text, was_truncated) = truncate_line(&m.line, GREP_MAX_LINE_LENGTH);
                lines_truncated |= was_truncated;
                output_lines.push(format!("{relative_path}:{}: {text}", m.line_number));
                continue;
            }

            // Context mode: pull surrounding lines from the file.
            let lines = file_cache.entry(m.path.clone()).or_insert_with(|| {
                std::fs::read_to_string(&m.path)
                    .map(|content| {
                        content
                            .replace("\r\n", "\n")
                            .split('\n')
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default()
            });
            if lines.is_empty() {
                output_lines.push(format!(
                    "{relative_path}:{}: (unable to read file)",
                    m.line_number
                ));
                continue;
            }
            let start = m.line_number.saturating_sub(args.context).max(1);
            let end = (m.line_number + args.context).min(lines.len() as u64);
            for current in start..=end {
                let line_text = lines
                    .get(usize::try_from(current - 1).unwrap_or(usize::MAX))
                    .map_or("", String::as_str);
                let (text, was_truncated) = truncate_line(line_text, GREP_MAX_LINE_LENGTH);
                lines_truncated |= was_truncated;
                // Match lines use `:`, context lines use `-` - the classic
                // grep convention, and what the model is trained on.
                if current == m.line_number {
                    output_lines.push(format!("{relative_path}:{current}: {text}"));
                } else {
                    output_lines.push(format!("{relative_path}-{current}- {text}"));
                }
            }
        }

        // ---- Byte cap + actionable notices --------------------------------
        // No line limit here: the match limit already capped the row count.
        let raw_output = output_lines.join("\n");
        let truncation = truncate_head(
            &raw_output,
            TruncationOptions {
                max_lines: Some(usize::MAX),
                max_bytes: None,
            },
        );
        let mut output = truncation.content;
        let mut details = serde_json::Map::new();
        let mut notices: Vec<String> = Vec::new();

        if outcome.limit_reached {
            notices.push(format!(
                "{effective_limit} matches limit reached. Use limit={} for more, or refine pattern",
                effective_limit * 2
            ));
            details.insert("matchLimitReached".into(), json!(effective_limit));
        }
        if truncation.truncated {
            notices.push(format!("{} limit reached", format_size(DEFAULT_MAX_BYTES)));
            details.insert("truncated".into(), json!(true));
        }
        if lines_truncated {
            notices.push(format!(
                "Some lines truncated to {GREP_MAX_LINE_LENGTH} chars. Use read tool to see full lines"
            ));
            details.insert("linesTruncated".into(), json!(true));
        }
        if !notices.is_empty() {
            output.push_str(&format!("\n\n[{}]", notices.join(". ")));
        }

        Ok(AgentToolResult {
            content: vec![cupel_core::types::ToolResultContent::Text(
                cupel_core::types::TextContent::plain(output),
            )],
            details: (!details.is_empty()).then_some(Value::Object(details)),
            terminate: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::GrepSearch;

    fn temp_root(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("cupel-greptool-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    async fn run_tool(root: &Path, args: Value) -> AgentToolResult {
        let tool = GrepTool::new(root, Arc::new(GrepSearch::new(root)));
        tool.execute("call_1", args, CancellationToken::new(), None)
            .await
            .unwrap()
    }

    fn text_of(result: &AgentToolResult) -> String {
        result
            .content
            .iter()
            .filter_map(|c| match c {
                cupel_core::types::ToolResultContent::Text(t) => Some(t.text.clone()),
                cupel_core::types::ToolResultContent::Image(_) => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn formats_matches_with_relative_paths() {
        let root = temp_root("format");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/main.rs"), "fn main() {}\n").unwrap();
        let result = run_tool(&root, json!({"pattern": "fn main"})).await;
        assert_eq!(text_of(&result), "src/main.rs:1: fn main() {}");
    }

    #[tokio::test]
    async fn no_matches_message() {
        let root = temp_root("nomatch");
        std::fs::write(root.join("a.txt"), "nothing here\n").unwrap();
        let result = run_tool(&root, json!({"pattern": "unfindable_xyz"})).await;
        assert_eq!(text_of(&result), "No matches found");
    }

    #[tokio::test]
    async fn context_lines_use_dash_separator() {
        let root = temp_root("context");
        std::fs::write(root.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        let result = run_tool(&root, json!({"pattern": "two", "context": 1})).await;
        let text = text_of(&result);
        assert!(text.contains("a.txt-1- one"), "got: {text}");
        assert!(text.contains("a.txt:2: two"), "got: {text}");
        assert!(text.contains("a.txt-3- three"), "got: {text}");
    }

    #[tokio::test]
    async fn limit_notice_appears() {
        let root = temp_root("limitnotice");
        std::fs::write(root.join("a.txt"), "x\n".repeat(10)).unwrap();
        let result = run_tool(&root, json!({"pattern": "x", "limit": 3})).await;
        let text = text_of(&result);
        assert!(text.contains("3 matches limit reached"), "got: {text}");
        assert!(text.contains("limit=6"), "got: {text}");
    }

    #[tokio::test]
    async fn invalid_args_are_an_error() {
        let root = temp_root("badargs");
        let tool = GrepTool::new(&root, Arc::new(GrepSearch::new(&root)));
        let result = tool
            .execute(
                "call_1",
                json!({"pattern": 42}),
                CancellationToken::new(),
                None,
            )
            .await;
        assert!(result.is_err());
    }
}
