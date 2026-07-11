//! Lifecycle hooks: user-provided executables that run on session events.
//!
//! Discovery is file-based, no config parsing: every EXECUTABLE file in
//! `<root>/hooks/<event>/` runs when that event fires, where the roots are
//! the cupel home (`~/.cupel`) and the project's `.cupel/`. Installing a
//! hook = dropping a script in a directory; uninstalling = deleting it.
//! That trivially machine-editable contract is what external integrations
//! (e.g. the `entire` CLI's agent protocol) build on.
//!
//! Events: `session-start`, `user-prompt-submit`, `stop`, `session-end`.
//! Each hook receives one JSON payload on stdin and must exit; stdout and
//! stderr are captured for debug logging, never shown in the UI. Hooks can
//! observe but not veto - a failing, missing, or slow hook is at most a
//! `tracing::warn`, never a broken session.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;

/// A lifecycle event plus its event-specific payload fields.
pub enum HookEvent<'a> {
    /// First prompt of this process (fires for resumed sessions too).
    SessionStart,
    /// Every prompt headed for the agent, steering included.
    UserPromptSubmit { prompt: &'a str },
    /// An agent run finished (the model stopped and tools are done).
    Stop,
    /// cupel is exiting normally.
    SessionEnd,
}

impl HookEvent<'_> {
    /// The directory name hooks for this event live in, doubling as the
    /// `event` field of the JSON payload.
    fn name(&self) -> &'static str {
        match self {
            Self::SessionStart => "session-start",
            Self::UserPromptSubmit { .. } => "user-prompt-submit",
            Self::Stop => "stop",
            Self::SessionEnd => "session-end",
        }
    }
}

/// The JSON object written to each hook's stdin. camelCase to match the
/// workspace-wide serde convention (cupel-core types.rs).
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct HookPayload {
    event: &'static str,
    session_id: String,
    /// Absolute path of the session's JSONL transcript.
    session_ref: String,
    cwd: String,
    timestamp: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    prompt: Option<String>,
}

/// Everything a dispatch needs, immutable and cheap to share into the
/// background tasks `fire_background` spawns.
struct HookConfig {
    /// Directories containing `hooks/` trees, in run order (home first,
    /// project second - same precedence direction as resource roots).
    roots: Vec<PathBuf>,
    session_id: String,
    session_ref: PathBuf,
    cwd: PathBuf,
    /// Per-hook wall clock budget; an expired hook is killed and warned.
    timeout: Duration,
}

/// Discovers and executes hooks for one session.
///
/// `stop` and steering-time events fire in the BACKGROUND (a TUI must not
/// freeze at run end), but strictly ordered: each background dispatch first
/// awaits the previous one, and [`HookRunner::settle`] lets the prompt path
/// wait for the chain to drain - so a `stop` hook is guaranteed to have
/// finished before the next prompt's hooks fire.
pub struct HookRunner {
    config: Arc<HookConfig>,
    /// The tail of the background dispatch chain.
    pending: Option<tokio::task::JoinHandle<()>>,
}

impl HookRunner {
    #[must_use]
    pub fn new(hook_roots: Vec<PathBuf>, session_id: &str, session_ref: &Path, cwd: &Path) -> Self {
        Self {
            config: Arc::new(HookConfig {
                roots: hook_roots,
                session_id: session_id.to_string(),
                session_ref: session_ref.to_path_buf(),
                cwd: cwd.to_path_buf(),
                timeout: Duration::from_mins(1),
            }),
            pending: None,
        }
    }

    /// Test hook: shrink the per-hook timeout (private - production always
    /// uses the default).
    #[cfg(test)]
    fn with_timeout(mut self, timeout: Duration) -> Self {
        Arc::get_mut(&mut self.config)
            .expect("no dispatch running yet")
            .timeout = timeout;
        self
    }

    /// Run every hook for `event` NOW, returning when all finished or timed
    /// out. Used for prompt-path events where the caller wants the
    /// guarantee (session-start, user-prompt-submit, session-end).
    pub async fn dispatch(&mut self, event: HookEvent<'_>) {
        self.settle().await;
        dispatch_event(
            &self.config,
            event.name(),
            payload_json(&self.config, &event),
        )
        .await;
    }

    /// Fire `event` without blocking the caller, chained after any earlier
    /// background dispatch so events never interleave or reorder.
    pub fn fire_background(&mut self, event: HookEvent<'_>) {
        let config = Arc::clone(&self.config);
        let name = event.name();
        // The payload is built NOW (borrowing the event's &str), so the
        // spawned task is 'static.
        let payload = payload_json(&self.config, &event);
        let previous = self.pending.take();
        self.pending = Some(tokio::spawn(async move {
            if let Some(previous) = previous {
                let _ = previous.await;
            }
            dispatch_event(&config, name, payload).await;
        }));
    }

    /// Await the background dispatch chain, if any is still running.
    pub async fn settle(&mut self) {
        if let Some(pending) = self.pending.take() {
            let _ = pending.await;
        }
    }
}

fn payload_json(config: &HookConfig, event: &HookEvent<'_>) -> String {
    let prompt = match event {
        HookEvent::UserPromptSubmit { prompt } => Some((*prompt).to_string()),
        _ => None,
    };
    let payload = HookPayload {
        event: event.name(),
        session_id: config.session_id.clone(),
        session_ref: config.session_ref.display().to_string(),
        cwd: config.cwd.display().to_string(),
        timestamp: cupel_core::types::now_ms(),
        prompt,
    };
    // A struct of strings cannot fail to serialize; fall back to {} anyway
    // rather than unwrap in a path that must never panic.
    serde_json::to_string(&payload).unwrap_or_else(|_| "{}".to_string())
}

/// Run all hooks for one event, sequentially in discovery order.
async fn dispatch_event(config: &HookConfig, event_name: &str, payload: String) {
    for script in hook_scripts(&config.roots, event_name) {
        run_one(&script, &payload, &config.cwd, config.timeout).await;
    }
}

/// All executable files in `<root>/hooks/<event>/` across the roots, each
/// directory's entries sorted by filename for a predictable run order.
/// Missing directories are simply empty - hooks are optional.
fn hook_scripts(roots: &[PathBuf], event_name: &str) -> Vec<PathBuf> {
    let mut scripts = Vec::new();
    for root in roots {
        let dir = root.join("hooks").join(event_name);
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        let mut in_dir: Vec<PathBuf> = entries
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| is_executable_file(p))
            .collect();
        in_dir.sort();
        scripts.extend(in_dir);
    }
    scripts
}

/// A regular file with any execute bit set. The execute-bit filter lets a
/// README.md or disabled-by-chmod script sit in the directory harmlessly.
fn is_executable_file(path: &Path) -> bool {
    let Ok(metadata) = std::fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Execute one hook: JSON on stdin, output captured, bounded by `timeout`.
/// Every failure mode (unspawnable, non-zero exit, timeout) is warn-only.
async fn run_one(script: &Path, payload: &str, cwd: &Path, timeout: Duration) {
    use tokio::io::AsyncWriteExt as _;

    let mut command = tokio::process::Command::new(script);
    command
        .current_dir(cwd)
        .stdin(std::process::Stdio::piped())
        // Captured, not inherited: a hook writing to the terminal would
        // corrupt the ratatui screen.
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        // If the timeout drops the child future, the process dies with it
        // instead of leaking.
        .kill_on_drop(true);

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => {
            tracing::warn!(hook = %script.display(), "hook failed to start: {e}");
            return;
        }
    };

    // Write the payload and CLOSE stdin (drop) so `cat`-style hooks see EOF.
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(payload.as_bytes()).await;
        drop(stdin);
    }

    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(output)) => {
            if output.status.success() {
                tracing::debug!(hook = %script.display(), "hook completed");
            } else {
                tracing::warn!(
                    hook = %script.display(),
                    status = %output.status,
                    stderr = %String::from_utf8_lossy(&output.stderr),
                    "hook exited non-zero"
                );
            }
        }
        Ok(Err(e)) => tracing::warn!(hook = %script.display(), "hook failed: {e}"),
        Err(_) => {
            // The timeout dropped the child future; kill_on_drop reaps it.
            tracing::warn!(hook = %script.display(), ?timeout, "hook timed out and was killed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cupel-hooks-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Write an executable shell script into `<root>/hooks/<event>/<name>`.
    #[cfg(unix)]
    fn install_script(root: &Path, event: &str, name: &str, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = root.join("hooks").join(event);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    fn runner(roots: Vec<PathBuf>, cwd: &Path) -> HookRunner {
        HookRunner::new(roots, "cupel-test", &cwd.join("t.jsonl"), cwd)
    }

    #[cfg(unix)]
    #[test]
    fn discovery_is_sorted_and_skips_non_executables() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = temp_root("discovery");
        install_script(&root, "stop", "b-second", "true");
        install_script(&root, "stop", "a-first", "true");
        // Not executable: must be skipped even though it's in the directory.
        let disabled = root.join("hooks/stop/c-disabled");
        std::fs::write(&disabled, "#!/bin/sh\ntrue\n").unwrap();
        std::fs::set_permissions(&disabled, std::fs::Permissions::from_mode(0o644)).unwrap();

        let scripts = hook_scripts(std::slice::from_ref(&root), "stop");
        let names: Vec<_> = scripts
            .iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap().to_string())
            .collect();
        assert_eq!(names, vec!["a-first", "b-second"]);
        // Missing event dir: empty, not an error.
        assert!(hook_scripts(&[root], "session-end").is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn dispatch_delivers_the_payload_on_stdin() {
        let root = temp_root("payload");
        let out = root.join("captured.json");
        install_script(
            &root,
            "user-prompt-submit",
            "capture",
            &format!("cat > {}", out.display()),
        );

        let mut runner = runner(vec![root.clone()], &root);
        runner
            .dispatch(HookEvent::UserPromptSubmit { prompt: "fix it" })
            .await;

        let captured: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
        assert_eq!(captured["event"], "user-prompt-submit");
        assert_eq!(captured["sessionId"], "cupel-test");
        assert_eq!(captured["prompt"], "fix it");
        assert!(
            captured["sessionRef"]
                .as_str()
                .unwrap()
                .ends_with("t.jsonl")
        );
        assert!(captured["timestamp"].as_u64().unwrap() > 0);
        // Non-prompt events omit the field entirely.
        install_script(
            &root,
            "stop",
            "capture",
            &format!("cat > {}", out.display()),
        );
        runner.dispatch(HookEvent::Stop).await;
        let captured: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&out).unwrap()).unwrap();
        assert_eq!(captured["event"], "stop");
        assert!(captured.get("prompt").is_none());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failing_and_unspawnable_hooks_are_harmless() {
        let root = temp_root("failing");
        install_script(&root, "stop", "fails", "exit 1");
        // An executable that isn't a valid program (spawn succeeds via sh
        // shebang absence -> exec error path).
        let mut runner = runner(vec![root.clone(), PathBuf::from("/nonexistent")], &root);
        runner.dispatch(HookEvent::Stop).await; // must simply return
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn slow_hooks_are_killed_at_the_timeout() {
        let root = temp_root("timeout");
        let marker = root.join("finished");
        install_script(
            &root,
            "stop",
            "sleeper",
            &format!("sleep 5\ntouch {}", marker.display()),
        );

        let mut runner = runner(vec![root.clone()], &root).with_timeout(Duration::from_millis(100));
        let start = std::time::Instant::now();
        runner.dispatch(HookEvent::Stop).await;
        assert!(start.elapsed() < Duration::from_secs(2), "must not wait 5s");
        assert!(!marker.exists(), "hook must have been killed");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn background_fires_stay_ordered_and_settle_waits() {
        let root = temp_root("ordering");
        let log = root.join("order.log");
        install_script(
            &root,
            "stop",
            "log",
            &format!("echo stop >> {}", log.display()),
        );
        install_script(
            &root,
            "session-end",
            "log",
            &format!("echo session-end >> {}", log.display()),
        );

        let mut runner = runner(vec![root.clone()], &root);
        runner.fire_background(HookEvent::Stop);
        runner.fire_background(HookEvent::SessionEnd);
        runner.settle().await;

        let order = std::fs::read_to_string(&log).unwrap();
        assert_eq!(order, "stop\nsession-end\n", "fire order must hold");
    }
}
