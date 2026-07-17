//! User settings: `settings.json` in `~/.cupel` (global) and
//! `<project>/.cupel` (per project), merged FIELD BY FIELD with the
//! project side winning - a project can pin the model while the thinking
//! level still comes from the global file.
//!
//! ```json
//! {
//!   "model": "claude-haiku-4-5",
//!   "thinking": "medium",
//!   "limits": { "maxCostUsd": 5.0, "maxTotalTokens": 2000000 }
//! }
//! ```
//!
//! Precedence: CLI flags (`--model`, `--thinking`) always beat settings;
//! settings beat the built-in defaults (credential-order model pick, no
//! thinking). Limits apply per SESSION: once the running cost or the
//! summed input+output tokens cross a limit, the TUI refuses new prompts
//! until a fresh session starts (`/new`, `/hot-reload`) or the limit is
//! raised. Loaded by the bootstrap loader, so startup and `/hot-reload`
//! see the same values.

use std::path::Path;

use cupel_core::types::ThinkingLevel;
use serde::Deserialize;

/// The parsed settings file(s). Every field optional: an empty `{}` (or a
/// missing file) changes nothing.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Settings {
    /// Default model id (must exist in the merged catalog).
    pub model: Option<String>,
    /// Default thinking level: off|minimal|low|medium|high|xhigh.
    pub thinking: Option<String>,
    pub limits: UsageLimits,
}

/// Per-session usage ceilings. `None` = unlimited.
#[derive(Debug, Clone, Copy, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct UsageLimits {
    pub max_cost_usd: Option<f64>,
    /// Input + output tokens summed (cache reads not counted - they are
    /// nearly free and would make the ceiling fire misleadingly early).
    pub max_total_tokens: Option<u64>,
}

impl UsageLimits {
    /// The first exceeded limit as a human-readable reason, or `None`
    /// while within budget.
    #[must_use]
    pub fn exceeded(&self, cost_usd: f64, total_tokens: u64) -> Option<String> {
        if let Some(max) = self.max_cost_usd
            && cost_usd >= max
        {
            return Some(format!(
                "session cost limit reached (${cost_usd:.2} of ${max:.2})"
            ));
        }
        if let Some(max) = self.max_total_tokens
            && total_tokens >= max
        {
            return Some(format!(
                "session token limit reached ({total_tokens} of {max})"
            ));
        }
        None
    }
}

/// Load `home/settings.json` then `<cwd>/.cupel/settings.json` and merge,
/// project side winning per FIELD. Malformed files are reported on stderr
/// and skipped (warn-and-continue); missing files are empty settings.
#[must_use]
pub fn load_settings(home: Option<&Path>, cwd: &Path) -> Settings {
    let mut merged = Settings::default();
    let mut paths = Vec::new();
    if let Some(home) = home {
        paths.push(home.join("settings.json"));
    }
    paths.push(cwd.join(".cupel/settings.json"));

    for path in paths {
        match read_settings_file(&path) {
            Ok(Some(layer)) => merged = merge(merged, layer),
            Ok(None) => {}
            Err(e) => eprintln!("warning: ignoring settings file: {e}"),
        }
    }
    merged
}

/// `Ok(None)` = file absent; `Err` = present but unreadable/unparsable (a
/// config the user wrote deserves a visible failure, same as models.json).
fn read_settings_file(path: &Path) -> Result<Option<Settings>, String> {
    let content = match std::fs::read_to_string(path) {
        Ok(content) => content,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    serde_json::from_str(&content)
        .map(Some)
        .map_err(|e| format!("{} is not valid: {e}", path.display()))
}

/// Field-by-field merge: `over`'s Some fields replace `base`'s.
fn merge(base: Settings, over: Settings) -> Settings {
    Settings {
        model: over.model.or(base.model),
        thinking: over.thinking.or(base.thinking),
        limits: UsageLimits {
            max_cost_usd: over.limits.max_cost_usd.or(base.limits.max_cost_usd),
            max_total_tokens: over
                .limits
                .max_total_tokens
                .or(base.limits.max_total_tokens),
        },
    }
}

/// The one thinking-level vocabulary, shared by `--thinking` and the
/// settings file. `Ok(None)` means "off" (thinking disabled).
pub fn parse_thinking(value: &str) -> Result<Option<ThinkingLevel>, String> {
    match value {
        "off" => Ok(None),
        "minimal" => Ok(Some(ThinkingLevel::Minimal)),
        "low" => Ok(Some(ThinkingLevel::Low)),
        "medium" => Ok(Some(ThinkingLevel::Medium)),
        "high" => Ok(Some(ThinkingLevel::High)),
        "xhigh" => Ok(Some(ThinkingLevel::XHigh)),
        other => Err(format!("unknown thinking level: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cupel-settings-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn settings_parse_camel_case_and_default_empty() {
        let settings: Settings = serde_json::from_str(
            r#"{"model": "m1", "thinking": "high", "limits": {"maxCostUsd": 2.5, "maxTotalTokens": 1000}}"#,
        )
        .unwrap();
        assert_eq!(settings.model.as_deref(), Some("m1"));
        assert_eq!(settings.thinking.as_deref(), Some("high"));
        assert_eq!(settings.limits.max_cost_usd, Some(2.5));
        assert_eq!(settings.limits.max_total_tokens, Some(1000));
        // `{}` is a valid file that changes nothing.
        assert_eq!(
            serde_json::from_str::<Settings>("{}").unwrap(),
            Settings::default()
        );
    }

    #[test]
    fn project_settings_override_home_per_field() {
        let root = temp_root("merge");
        let (home, cwd) = (root.join("home"), root.join("proj"));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(cwd.join(".cupel")).unwrap();
        std::fs::write(
            home.join("settings.json"),
            r#"{"model": "global-model", "thinking": "low", "limits": {"maxCostUsd": 10}}"#,
        )
        .unwrap();
        // The project pins the model and tightens cost; thinking untouched.
        std::fs::write(
            cwd.join(".cupel/settings.json"),
            r#"{"model": "project-model", "limits": {"maxCostUsd": 2}}"#,
        )
        .unwrap();

        let settings = load_settings(Some(&home), &cwd);
        assert_eq!(settings.model.as_deref(), Some("project-model"));
        assert_eq!(settings.thinking.as_deref(), Some("low"), "kept from home");
        assert_eq!(settings.limits.max_cost_usd, Some(2.0), "project wins");
        assert_eq!(settings.limits.max_total_tokens, None);
    }

    #[test]
    fn malformed_settings_are_skipped_not_fatal() {
        let root = temp_root("bad");
        let (home, cwd) = (root.join("home"), root.join("proj"));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(home.join("settings.json"), "{nope").unwrap();
        assert_eq!(load_settings(Some(&home), &cwd), Settings::default());
    }

    #[test]
    fn thinking_vocabulary_matches_the_cli() {
        assert_eq!(parse_thinking("off").unwrap(), None);
        assert_eq!(parse_thinking("xhigh").unwrap(), Some(ThinkingLevel::XHigh));
        assert!(parse_thinking("turbo").is_err());
    }

    #[test]
    fn limits_report_the_first_exceeded_ceiling() {
        let limits = UsageLimits {
            max_cost_usd: Some(1.0),
            max_total_tokens: Some(100),
        };
        assert!(limits.exceeded(0.5, 50).is_none());
        assert!(limits.exceeded(1.0, 0).unwrap().contains("cost limit"));
        assert!(limits.exceeded(0.0, 100).unwrap().contains("token limit"));
        assert!(UsageLimits::default().exceeded(999.0, u64::MAX).is_none());
    }
}
