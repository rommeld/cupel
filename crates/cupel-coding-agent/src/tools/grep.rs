//! The `grep` tool.
//!
//! Split of responsibilities,
//! * `search` finds matching lines (the pluggable backend).
//! * This module is the model facing layer, argument schema, context lines,
//!   line/byte truncation, and the output format the model sees
//!   (path, line number and text separated by colons for matches, path, line number and text separated by hyphens for context).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use cupel_agent::types::{AgentTool, AgentToolResult, ToolError, ToolUpdateFn};

use crate::search::{
    CodeSearch, SearchLimit, SearchMatch, SearchQuery, resolve_to_root, source_locator,
};
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

/// Tool arguments. Deserializing into this struct is the argument
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
                which files to read. Returned paths open directly with read from the current \
                working directory, even for scoped searches. Locations are navigation hints, \
                not versioned source observations; re-read before editing.",
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

        // The search scope affects ranking, never the meaning of a result path.
        let search_path = resolve_to_root(args.path.as_deref().unwrap_or("."), &self.cwd);

        // The scope, spelled out for the no match message, a model that sees
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
        // like, the ranker reads that off the query before the backend
        // takes ownership of it.
        let ranker = (args.output_mode == OutputMode::Files).then(|| Ranker::new(&query));
        let outcome = self.backend.search(query, cancel.clone()).await?;

        if outcome.matches.is_empty() {
            return Ok(AgentToolResult::text(format!(
                "No matches found for {scope}"
            )));
        }

        let format_path = |path: &Path| source_locator(path, &self.cwd);

        if let Some(ranker) = ranker {
            let ranked = ranker.rank(&outcome.matches, &search_path);
            return Ok(render_files(
                &ranked,
                outcome.limit_reached,
                effective_limit,
                format_path,
            ));
        }

        let (output_lines, lines_truncated, context_changed) =
            render_content(&outcome.matches, args.context, format_path, &cancel)?;

        // No line limit here, the match limit already capped the row count.
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
        if context_changed {
            notices.push("Context unavailable or changed since search. Match text is from search; use read for current source".into());
            details.insert("contextChanged".into(), json!(true));
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

/// Union context intervals before rendering, keeping the union of match markers.
/// Match text remains the backend observation, not a later reread of that line.
fn render_content(
    matches: &[SearchMatch],
    context: u64,
    format_path: impl Fn(&Path) -> String,
    cancel: &CancellationToken,
) -> Result<(Vec<String>, bool, bool), ToolError> {
    let mut files: BTreeMap<&Path, BTreeMap<u64, &str>> = BTreeMap::new();
    for m in matches {
        files
            .entry(&m.path)
            .or_default()
            .insert(m.line_number, &m.line);
    }
    let mut output = Vec::new();
    let mut output_bytes = 0_usize;
    let mut lines_truncated = false;
    let mut context_changed = false;
    'files: for (path, hits) in files {
        if cancel.is_cancelled() {
            return Err("Operation aborted".into());
        }
        let locator = format_path(path);
        let content = (context > 0).then(|| std::fs::read_to_string(path));
        let lines: Vec<&str> = match &content {
            Some(Ok(content)) => {
                // Match the search backend's removal of a leading UTF-8 BOM.
                content
                    .strip_prefix('\u{feff}')
                    .unwrap_or(content)
                    .lines()
                    .collect()
            }
            Some(Err(_)) => {
                context_changed = true;
                Vec::new()
            }
            None => Vec::new(),
        };
        let mut windows = Vec::new();
        for (&line, &text) in &hits {
            let current = usize::try_from(line.saturating_sub(1))
                .ok()
                .and_then(|i| lines.get(i));
            let (start, end) = if context > 0 && current.copied() == Some(text) {
                (
                    line.saturating_sub(context).max(1),
                    line.saturating_add(context).min(lines.len() as u64),
                )
            } else {
                context_changed |= context > 0;
                (line, line)
            };
            windows.push((start, end));
        }
        windows.sort_unstable();
        let mut intervals: Vec<(u64, u64)> = Vec::new();
        for (start, end) in windows {
            if let Some((_, previous_end)) = intervals.last_mut()
                && start <= previous_end.saturating_add(1)
            {
                *previous_end = (*previous_end).max(end);
            } else {
                intervals.push((start, end));
            }
        }
        for (start, end) in intervals {
            for line in start..=end {
                if cancel.is_cancelled() {
                    return Err("Operation aborted".into());
                }
                let source = hits.get(&line).copied().unwrap_or_else(|| {
                    usize::try_from(line - 1)
                        .ok()
                        .and_then(|i| lines.get(i))
                        .copied()
                        .unwrap_or("")
                });
                let (text, truncated) = truncate_line(source, GREP_MAX_LINE_LENGTH);
                lines_truncated |= truncated;
                let separator = if hits.contains_key(&line) { ':' } else { '-' };
                let row = format!("{locator}{separator}{line}{separator} {text}");
                output_bytes =
                    output_bytes.saturating_add(row.len() + usize::from(!output.is_empty()));
                output.push(row);
                // Keep one overflow row so the usual truncation path emits its
                // notice, without building unbounded context output in memory.
                if output_bytes > DEFAULT_MAX_BYTES {
                    break 'files;
                }
            }
        }
    }
    Ok((output, lines_truncated, context_changed))
}

/// `files` mode output, the best `limit` files, one line each.
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
    async fn utf8_bom_preserves_context_without_reporting_source_changes() {
        let root = temp_root("bom-context");
        for newline in ["\n", "\r\n"] {
            std::fs::write(
                root.join("a.txt"),
                format!("\u{feff}hit{newline}context{newline}"),
            )
            .unwrap();
            let result = run_tool(&root, json!({"pattern": "hit", "context": 1})).await;
            assert_eq!(text_of(&result), "a.txt:1: hit\na.txt-2- context");
            assert!(
                result.details.is_none(),
                "unexpected notices: {:?}",
                result.details
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn scoped_locators_round_trip_to_read_in_both_modes() {
        use crate::tools::read::ReadTool;

        let root = temp_root("locators");
        let external = temp_root("external-locators");
        std::fs::create_dir_all(root.join("nested")).unwrap();
        std::fs::write(root.join("nested/same.txt"), "needle local\n").unwrap();
        std::fs::write(external.join("same.txt"), "needle external\n").unwrap();
        for (scope, expected) in [
            ("nested".to_string(), root.join("nested/same.txt")),
            ("nested/same.txt".to_string(), root.join("nested/same.txt")),
            (external.display().to_string(), external.join("same.txt")),
            (
                external.join("same.txt").display().to_string(),
                external.join("same.txt"),
            ),
        ] {
            for mode in ["content", "files"] {
                let found = run_tool(
                    &root,
                    json!({"pattern": "needle", "path": scope, "outputMode": mode}),
                )
                .await;
                let text = text_of(&found);
                let locator = text.split_once(":1").unwrap().0;
                let read = ReadTool::new(&root)
                    .execute(
                        "read-hit",
                        json!({"path": locator}),
                        CancellationToken::new(),
                        None,
                    )
                    .await
                    .unwrap();
                let actual =
                    PathBuf::from(read.details.as_ref().unwrap()["path"].as_str().unwrap());
                assert_eq!(
                    actual,
                    expected.canonicalize().unwrap(),
                    "scope={scope}, mode={mode}, locator={locator}"
                );
                assert!(text_of(&read).contains("Coverage: complete file"));
            }
        }
    }

    #[tokio::test]
    async fn overlapping_context_is_merged_without_losing_match_markers() {
        let root = temp_root("merged-context");
        std::fs::write(
            root.join("a.txt"),
            "one\r\nhit two\r\nhit three\r\nhit four\r\nfive\r\n",
        )
        .unwrap();
        let result = run_tool(&root, json!({"pattern": "hit", "context": 1})).await;
        assert_eq!(
            text_of(&result),
            "a.txt-1- one\na.txt:2: hit two\na.txt:3: hit three\na.txt:4: hit four\na.txt-5- five"
        );
        assert!(result.details.is_none());
    }

    #[tokio::test]
    async fn disjoint_context_and_files_keep_source_order() {
        let root = temp_root("disjoint-context");
        std::fs::write(root.join("a.txt"), "a\nhit\nc\nskipped\ne\nhit\ng\n").unwrap();
        std::fs::write(root.join("b.txt"), "hit\n").unwrap();
        let result = run_tool(&root, json!({"pattern": "hit", "context": 1})).await;
        assert_eq!(
            text_of(&result),
            "a.txt-1- a\na.txt:2: hit\na.txt-3- c\na.txt-5- e\na.txt:6: hit\na.txt-7- g\nb.txt:1: hit"
        );
    }

    #[tokio::test]
    async fn merged_context_preserves_byte_and_line_truncation_notices() {
        let root = temp_root("context-caps");
        std::fs::write(
            root.join("a.txt"),
            format!("hit\n{}\n", "x".repeat(600)).repeat(200),
        )
        .unwrap();
        let result = run_tool(
            &root,
            json!({"pattern": "hit", "context": u64::MAX, "limit": 2}),
        )
        .await;
        let text = text_of(&result);
        assert!(text.contains("2 matches limit reached"), "{text}");
        assert!(text.contains("50.0KB limit reached"));
        assert!(text.contains("Some lines truncated to 500 chars"));
        let details = result.details.unwrap();
        assert_eq!(details["truncated"], true);
        assert_eq!(details["linesTruncated"], true);
        assert_eq!(details["matchLimitReached"], 2);
    }

    #[test]
    fn changed_or_unavailable_context_does_not_replace_the_observed_match() {
        let root = temp_root("changed-context");
        let path = root.join("a.txt");
        std::fs::write(&path, "one\nnew text\nhit\nfour\n").unwrap();
        let matches = vec![
            SearchMatch {
                path: path.clone(),
                line_number: 2,
                line: "old hit".to_string(),
            },
            SearchMatch {
                path: path.clone(),
                line_number: 3,
                line: "hit".to_string(),
            },
        ];
        let (rows, _, changed) = render_content(
            &matches,
            2,
            |path| source_locator(path, &root),
            &CancellationToken::new(),
        )
        .unwrap();
        assert!(changed);
        assert_eq!(
            rows,
            [
                "a.txt-1- one",
                "a.txt:2: old hit",
                "a.txt:3: hit",
                "a.txt-4- four"
            ]
        );
        std::fs::remove_file(&path).unwrap();
        let (rows, _, changed) = render_content(
            &matches,
            2,
            |path| source_locator(path, &root),
            &CancellationToken::new(),
        )
        .unwrap();
        assert!(changed);
        assert_eq!(rows, ["a.txt:2: old hit", "a.txt:3: hit"]);
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
