//! The `grep` tool.
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

use crate::search::{CodeSearch, SearchLimit, SearchQuery, resolve_to_root};
use crate::tools::grep_rank::{Bucket, FileSummary, Ranker};
use crate::truncate::{
    DEFAULT_MAX_BYTES, GREP_MAX_LINE_LENGTH, TruncationOptions, format_size, truncate_head,
    truncate_line,
};

const DEFAULT_LIMIT: usize = 100;
const FILES_DEFAULT_LIMIT: usize = 10;
const FILES_PER_FILE_CAP: usize = 20;
const FILES_RANKING_WINDOW: usize = 500;
const FILES_PREVIEW_MAX_CHARS: usize = 120;

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
    #[serde(default)]
    context: u64,
    limit: Option<usize>,
    #[serde(default)]
    output_mode: OutputMode,
}

/// What the tool returns.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum OutputMode {
    #[default]
    Content,
    Files,
}

pub struct GrepTool {
    cwd: PathBuf,
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
                {GREP_MAX_LINE_LENGTH} chars. outputMode \"files\" returns one line per file \
                instead ({FILES_DEFAULT_LIMIT} by default), ranked with definitions first and \
                test code last, each with its best matching line as preview - use it to learn \
                which files to read.",
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
        // Kept in sync with `GrepArgs` by hand.
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
                    "description": "Maximum number of matches to return (default: 100); in outputMode files, maximum number of files shown (default: 10)"
                },
                "outputMode": {
                    "type": "string",
                    "enum": ["content", "files"],
                    "description": "content (default): matching lines. files: one line per file, ranked - definitions first, test code last - with the best matching line as preview"
                }
            },
            "required": ["pattern"]
        })
    }

    /// `grep /pattern in <path> (<glob>) limit N`.
    fn describe_call(&self, args: &Value) -> String {
        let Some(pattern) = args.get("pattern").and_then(Value::as_str) else {
            return self.name().to_string();
        };
        let path = args.get("path").and_then(Value::as_str).unwrap_or(".");
        let flag = if args.get("outputMode").and_then(Value::as_str) == Some("files") {
            "-l "
        } else {
            ""
        };
        let mut out = format!("grep {flag}/{pattern}/ in {path}");
        if let Some(glob) = args.get("glob").and_then(Value::as_str) {
            out.push_str(&format!(" ({glob})"));
        }
        if let Some(limit) = args.get("limit").and_then(Value::as_u64) {
            out.push_str(&format!(" limit {limit}"));
        }
        out
    }

    async fn execute(
        &self,
        _tool_call_id: &str,
        args: Value,
        cancel: CancellationToken,
        _on_update: Option<ToolUpdateFn>,
    ) -> Result<AgentToolResult, ToolError> {
        let args: GrepArgs = serde_json::from_value(args)?;
        let effective_limit = args
            .limit
            .unwrap_or(match args.output_mode {
                OutputMode::Content => DEFAULT_LIMIT,
                OutputMode::Files => FILES_DEFAULT_LIMIT,
            })
            .max(1);
        let limit = match args.output_mode {
            OutputMode::Content => SearchLimit::Matches(effective_limit),
            OutputMode::Files => SearchLimit::Files {
                max_files: effective_limit.max(FILES_RANKING_WINDOW),
                per_file: FILES_PER_FILE_CAP,
            },
        };

        // Where are we searching? Needed for both the backend query and for
        // making result paths relative + readable.
        let search_path = resolve_to_root(args.path.as_deref().unwrap_or("."), &self.cwd);
        let searching_directory = search_path.is_dir();

        // The scope, spelled out for the no-match message: a model that sees
        // what was searched where fixes a bad glob instead of retrying blind.
        let scope = format!(
            "/{}/ in {}{}",
            args.pattern,
            args.path.as_deref().unwrap_or("."),
            args.glob
                .as_deref()
                .map_or_else(String::new, |glob| format!(" ({glob})"))
        );
        let query = SearchQuery {
            pattern: args.pattern,
            path: args.path,
            glob: args.glob,
            ignore_case: args.ignore_case,
            literal: args.literal,
            limit,
        };
        // Files mode needs to know what a definition of the pattern looks
        // like; the ranker reads that off the query before the backend
        // takes ownership of it.
        let ranker = (args.output_mode == OutputMode::Files).then(|| Ranker::new(&query));
        let outcome = self.backend.search(query, cancel).await?;

        if outcome.matches.is_empty() {
            return Ok(AgentToolResult::text(format!(
                "No matches found for {scope}"
            )));
        }

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

        if let Some(ranker) = ranker {
            let ranked = ranker.rank(&outcome.matches, &search_path);
            return Ok(render_files(
                &ranked,
                outcome.limit_reached,
                effective_limit,
                format_path,
            ));
        }

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

/// `files` mode output: the best `limit` files, one line each.
fn render_files(
    ranked: &[FileSummary],
    window_full: bool,
    limit: usize,
    format_path: impl Fn(&Path) -> String,
) -> AgentToolResult {
    let total = ranked.len();
    let mut lines: Vec<String> = Vec::with_capacity(total.min(limit));
    for file in ranked.iter().take(limit) {
        let mut line = format!("{}:{}", format_path(&file.path), file.preview_line_number);
        if file.defines_name {
            line.push_str(" [def]");
        }
        let more = if file.matches >= FILES_PER_FILE_CAP {
            "+"
        } else {
            ""
        };
        let plural = if file.matches == 1 { "" } else { "es" };
        line.push_str(&format!(" ({}{more} match{plural}", file.matches));
        match file.bucket {
            Bucket::Source => {}
            Bucket::Test => line.push_str(", test"),
            Bucket::LowPriority => line.push_str(", low-priority"),
        }
        let (preview, _was_truncated) = truncate_line(file.preview.trim(), FILES_PREVIEW_MAX_CHARS);
        line.push_str(&format!(")  {preview}"));
        lines.push(line);
    }

    // Same byte cap as content mode. The line count is already bounded
    // by the file limit.
    let truncation = truncate_head(
        &lines.join("\n"),
        TruncationOptions {
            max_lines: Some(usize::MAX),
            max_bytes: None,
        },
    );
    let mut output = truncation.content;
    let mut details = serde_json::Map::new();
    let mut notices: Vec<String> = Vec::new();
    if total > limit || window_full {
        // Only lower bound.
        let more = if window_full { "+" } else { "" };
        notices.push(format!(
            "{} of {total}{more} files shown. Use limit={} for more, or refine pattern",
            lines.len(),
            limit * 2,
        ));
        details.insert("fileLimitReached".into(), json!(limit));
    }
    if truncation.truncated {
        notices.push(format!("{} limit reached", format_size(DEFAULT_MAX_BYTES)));
        details.insert("truncated".into(), json!(true));
    }
    if !notices.is_empty() {
        output.push_str(&format!("\n\n[{}]", notices.join(". ")));
    }

    AgentToolResult {
        content: vec![cupel_core::types::ToolResultContent::Text(
            cupel_core::types::TextContent::plain(output),
        )],
        details: (!details.is_empty()).then_some(Value::Object(details)),
        terminate: false,
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

    #[test]
    fn describe_call_reads_like_a_search() {
        let tool = GrepTool::new("/tmp", Arc::new(GrepSearch::new("/tmp")));
        assert_eq!(
            tool.describe_call(&json!({"pattern": "fn main"})),
            "grep /fn main/ in ."
        );
        assert_eq!(
            tool.describe_call(
                &json!({"pattern": "TODO", "path": "src", "glob": "*.rs", "limit": 50})
            ),
            "grep /TODO/ in src (*.rs) limit 50"
        );
        assert_eq!(tool.describe_call(&json!({"path": "src"})), "grep");
        assert_eq!(
            tool.describe_call(&json!({"pattern": "Foo", "outputMode": "files"})),
            "grep -l /Foo/ in ."
        );
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
        assert_eq!(
            text_of(&result),
            "No matches found for /unfindable_xyz/ in ."
        );
        std::fs::create_dir_all(root.join("src")).unwrap();
        let result = run_tool(
            &root,
            json!({"pattern": "unfindable_xyz", "path": "src", "glob": "*.rs"}),
        )
        .await;
        assert_eq!(
            text_of(&result),
            "No matches found for /unfindable_xyz/ in src (*.rs)"
        );
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

    #[tokio::test]
    async fn files_mode_ranks_definitions_first_and_tests_last() {
        let root = temp_root("filesmode");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("tests")).unwrap();
        std::fs::write(
            root.join("src/user.rs"),
            "use crate::Widget;\nfn go() {\n    Widget::new();\n}\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src/widget.rs"),
            "pub struct Widget {\n    id: u32,\n}\n",
        )
        .unwrap();
        std::fs::write(root.join("tests/it.rs"), "use app::Widget;\n").unwrap();
        let result = run_tool(&root, json!({"pattern": "Widget", "outputMode": "files"})).await;
        let text = text_of(&result);
        assert_eq!(
            text.lines().collect::<Vec<_>>(),
            [
                "src/widget.rs:1 [def] (1 match)  pub struct Widget {",
                "src/user.rs:1 (2 matches)  use crate::Widget;",
                "tests/it.rs:1 (1 match, test)  use app::Widget;",
            ],
            "got: {text}"
        );
        assert!(result.details.is_none());
    }

    #[tokio::test]
    async fn files_mode_limit_counts_files() {
        let root = temp_root("fileslimit");
        for name in ["a.txt", "b.txt", "c.txt"] {
            std::fs::write(root.join(name), "x\n".repeat(30)).unwrap();
        }
        let result = run_tool(
            &root,
            json!({"pattern": "x", "outputMode": "files", "limit": 2}),
        )
        .await;
        let text = text_of(&result);
        assert!(
            text.starts_with("a.txt:1 (20+ matches)  x\nb.txt:1 (20+ matches)  x"),
            "got: {text}"
        );
        assert!(text.contains("2 of 3 files shown"), "got: {text}");
        assert!(text.contains("limit=4"), "got: {text}");
        assert_eq!(result.details.unwrap()["fileLimitReached"], 2);
    }

    #[tokio::test]
    async fn files_mode_ranks_the_whole_window_before_applying_the_limit() {
        let root = temp_root("fileswindow");
        std::fs::write(root.join("a.rs"), "use crate::Widget;\n").unwrap();
        std::fs::write(root.join("z.rs"), "pub struct Widget;\n").unwrap();
        let result = run_tool(
            &root,
            json!({"pattern": "Widget", "outputMode": "files", "limit": 1}),
        )
        .await;
        let text = text_of(&result);
        assert!(
            text.starts_with("z.rs:1 [def] (1 match)  pub struct Widget;"),
            "got: {text}"
        );
        assert!(text.contains("1 of 2 files shown"), "got: {text}");
    }
}
