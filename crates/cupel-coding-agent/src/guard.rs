//! The bash denylist guard: cupel's first tool-execution guardrail.
//!
//! The agent loop asks [`AgentHooks::before_tool_call`] before EVERY tool
//! execution - a veto point that has existed in cupel-agent from the start
//! but was unused until now. [`BashGuard`] implements it for the `bash`
//! tool: a command matching any deny pattern is blocked, and the model
//! receives an error tool-result naming the pattern (so it can adapt
//! instead of retrying blindly).
//!
//! Patterns are REGEXES (the same engine as the grep tool), one per line:
//!
//! ```text
//! # ~/.cupel/bash-deny (global) or <project>/.cupel/bash-deny
//! git\s+push\s+--force
//! DROP\s+TABLE
//! ```
//!
//! The effective list is the UNION of the built-in defaults (`rm -rf` and
//! friends) and both files - deny rules from different layers never cancel
//! each other. Matching is deliberately conservative: every line of the
//! command is tested, and a match anywhere blocks - even inside a quoted
//! string (`echo "rm -rf"` is blocked too). A false positive costs one
//! polite error the model can rephrase around; a false negative costs the
//! user's files.

use std::path::Path;

use async_trait::async_trait;
use cupel_agent::{AgentHooks, types::BeforeToolCallResult};
use cupel_core::types::{AssistantMessage, ToolCall};
use grep_matcher::Matcher as _;

/// Built-in deny patterns, active even with no config files: the classic
/// recursive-force delete in its common spellings (`rm -rf`, `rm -fr`,
/// combined flag groups like `-Rf`, and a `sudo` prefix all match because
/// the regex anchors on the `rm` word, not the line start).
const DEFAULT_DENY: &[&str] = &[
    r"\brm\s+-[a-zA-Z]*[rR][a-zA-Z]*f",
    r"\brm\s+-[a-zA-Z]*f[a-zA-Z]*[rR]",
];

/// One compiled deny rule; the source text rides along for the error
/// message the model sees.
struct DenyRule {
    source: String,
    matcher: grep_regex::RegexMatcher,
}

/// [`AgentHooks`] implementation that vetoes denied bash commands.
/// Everything except `bash` passes through untouched.
pub struct BashGuard {
    rules: Vec<DenyRule>,
}

impl BashGuard {
    /// Compile a pattern list. Invalid regexes are reported on stderr and
    /// skipped (warn-and-continue): a typo in one rule must not disable
    /// the session OR silently drop the rest of the list.
    #[must_use]
    pub fn new(patterns: &[String]) -> Self {
        let rules = patterns
            .iter()
            .filter_map(
                |pattern| match grep_regex::RegexMatcherBuilder::new().build(pattern) {
                    Ok(matcher) => Some(DenyRule {
                        source: pattern.clone(),
                        matcher,
                    }),
                    Err(e) => {
                        eprintln!("warning: ignoring invalid bash-deny pattern \"{pattern}\": {e}");
                        None
                    }
                },
            )
            .collect();
        Self { rules }
    }

    /// The production constructor: built-in defaults + `~/.cupel/bash-deny`
    /// + `<cwd>/.cupel/bash-deny`.
    #[must_use]
    pub fn from_config(home: Option<&Path>, cwd: &Path) -> Self {
        let mut patterns: Vec<String> = DEFAULT_DENY.iter().map(ToString::to_string).collect();
        if let Some(home) = home {
            patterns.extend(read_deny_file(&home.join("bash-deny")));
        }
        patterns.extend(read_deny_file(&cwd.join(".cupel/bash-deny")));
        Self::new(&patterns)
    }

    /// The first rule matching any line of `command`, if any. Line-by-line
    /// because the grep engine is line-oriented and multi-line commands
    /// (heredocs, `&&` continuations) must not smuggle a denied command
    /// past the guard on line two.
    fn deny_match(&self, command: &str) -> Option<&str> {
        for rule in &self.rules {
            for line in command.lines() {
                if rule.matcher.is_match(line.as_bytes()).unwrap_or(false) {
                    return Some(&rule.source);
                }
            }
        }
        None
    }
}

/// Read one deny file: a pattern per line, `#` comments and blank lines
/// skipped. A missing file is an empty list (the resources.rs idiom).
fn read_deny_file(path: &Path) -> Vec<String> {
    let Ok(content) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    content
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(ToString::to_string)
        .collect()
}

#[async_trait]
impl AgentHooks for BashGuard {
    async fn before_tool_call(
        &self,
        _assistant: &AssistantMessage,
        tool_call: &ToolCall,
    ) -> Option<BeforeToolCallResult> {
        if tool_call.name != "bash" {
            return None;
        }
        let command = tool_call.arguments.get("command")?.as_str()?;
        let pattern = self.deny_match(command)?;
        tracing::warn!(pattern, command, "bash command blocked by denylist");
        Some(BeforeToolCallResult {
            block: true,
            // Addressed to the MODEL: name the rule and point at a way
            // forward, so it does not retry the same command verbatim.
            reason: Some(format!(
                "Command blocked by cupel's bash denylist (pattern: {pattern}). \
                 Do not retry this command; choose a safer alternative or ask \
                 the user to run it themselves."
            )),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cupel-guard-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn default_guard() -> BashGuard {
        let patterns: Vec<String> = DEFAULT_DENY.iter().map(ToString::to_string).collect();
        BashGuard::new(&patterns)
    }

    #[test]
    fn default_patterns_catch_recursive_force_deletes() {
        let guard = default_guard();
        for denied in [
            "rm -rf /",
            "rm -fr target",
            "rm -Rf .",
            "sudo rm -rf /var",
            "cd /tmp && rm -rf cache",
            "echo done\nrm -rf /", // second line must not slip through
        ] {
            assert!(guard.deny_match(denied).is_some(), "should block: {denied}");
        }
        for allowed in ["rm -r target", "rm file.txt", "cargo build", "grep rf ."] {
            assert!(
                guard.deny_match(allowed).is_none(),
                "should pass: {allowed}"
            );
        }
    }

    #[test]
    fn config_files_extend_the_defaults() {
        let root = temp_root("layers");
        let (home, cwd) = (root.join("home"), root.join("proj"));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(cwd.join(".cupel")).unwrap();
        std::fs::write(
            home.join("bash-deny"),
            "# comment\n\ngit\\s+push\\s+--force\n",
        )
        .unwrap();
        std::fs::write(cwd.join(".cupel/bash-deny"), "DROP\\s+TABLE\n").unwrap();

        let guard = BashGuard::from_config(Some(&home), &cwd);
        // Union: defaults AND both layers are all active.
        assert!(guard.deny_match("rm -rf /").is_some(), "defaults kept");
        assert!(guard.deny_match("git push --force").is_some(), "home rule");
        assert!(guard.deny_match("psql -c 'DROP TABLE x'").is_some());
        assert!(guard.deny_match("git push").is_none());
    }

    #[test]
    fn invalid_patterns_are_skipped_not_fatal() {
        let guard = BashGuard::new(&["[unclosed".to_string(), r"\brm\s+-rf".to_string()]);
        // The bad rule is dropped; the good one still guards.
        assert!(guard.deny_match("rm -rf /").is_some());
        assert!(guard.deny_match("[unclosed").is_none());
    }

    #[tokio::test]
    async fn hook_blocks_bash_only_and_names_the_pattern() {
        use cupel_core::types::{Api, Provider, StopReason, Usage};

        let guard = default_guard();
        let assistant = AssistantMessage {
            content: Vec::new(),
            api: Api::from("mock"),
            provider: Provider::from("mock"),
            model: "mock".into(),
            response_model: None,
            response_id: None,
            usage: Usage::default(),
            stop_reason: StopReason::ToolUse,
            error_message: None,
            timestamp: cupel_core::types::now_ms(),
        };
        let call = |name: &str, args: serde_json::Value| ToolCall {
            id: "call_1".into(),
            name: name.into(),
            arguments: args,
            thought_signature: None,
        };

        // Denied bash command: blocked, reason names the pattern.
        let verdict = guard
            .before_tool_call(
                &assistant,
                &call("bash", serde_json::json!({"command": "rm -rf /"})),
            )
            .await
            .expect("verdict");
        assert!(verdict.block);
        assert!(verdict.reason.unwrap().contains("denylist"));

        // Harmless bash command and non-bash tools pass untouched.
        let ok = guard
            .before_tool_call(
                &assistant,
                &call("bash", serde_json::json!({"command": "ls -la"})),
            )
            .await;
        assert!(ok.is_none());
        let write = guard
            .before_tool_call(
                &assistant,
                &call(
                    "write",
                    serde_json::json!({"path": "rm -rf", "content": ""}),
                ),
            )
            .await;
        assert!(write.is_none(), "guard only inspects bash");
    }
}
