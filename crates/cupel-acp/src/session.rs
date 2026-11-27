//! One session's agent, tools, transcript, and lifecycle hooks.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent_client_protocol::v1::schema as acp;
use agent_client_protocol::{Error, Result};
use cupel_agent::{Agent, AgentEvent, AgentMessage, AgentOptions};
use cupel_coding_agent::session::{self, SessionRecorder};
use cupel_coding_agent::settings::DEFAULT_PRESET;
use cupel_coding_agent::{bootstrap, startup};
use cupel_core::provider::Registry;
use futures_util::StreamExt as _;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::events::Mapper;

pub type Notify = Arc<dyn Fn(acp::SessionUpdate) -> Result<()> + Send + Sync>;

pub fn invalid(message: impl Into<String>) -> Error {
    Error::new(-32602, message)
}

pub fn failure(message: impl Into<String>) -> Error {
    Error::new(-32603, message)
}

pub fn directory(path: &Path) -> Result<PathBuf> {
    if !path.is_absolute() {
        return Err(invalid("session cwd must be absolute"));
    }
    let cwd = path
        .canonicalize()
        .map_err(|error| invalid(format!("cannot open session cwd: {error}")))?;
    if !cwd.is_dir() {
        return Err(invalid("session cwd must be a directory"));
    }
    Ok(cwd)
}

pub fn prompt_text(blocks: &[acp::ContentBlock]) -> Result<String> {
    let mut text = String::new();
    for block in blocks {
        let acp::ContentBlock::Text(block) = block else {
            return Err(invalid(
                "cupel-acp currently accepts text prompt blocks only",
            ));
        };
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&block.text);
        if text.len() > 1024 * 1024 {
            return Err(invalid("prompt exceeds 1 MiB"));
        }
    }
    if text.trim().is_empty() {
        return Err(invalid("prompt must contain text"));
    }
    Ok(text)
}

pub struct RestoredSession {
    model: String,
    messages: Vec<AgentMessage>,
}

impl RestoredSession {
    pub fn load(home: Option<&Path>, cwd: &Path, id: &str) -> Result<Self> {
        if id.is_empty()
            || id.len() > 128
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(invalid("invalid session id"));
        }
        let path = session::sessions_dir(home, cwd)
            .ok_or_else(|| invalid("session persistence requires a Cupel home"))?
            .join(format!("{id}.jsonl"));
        let (header, messages) = session::load_transcript(&path).map_err(invalid)?;
        if header.session_id != id || directory(Path::new(&header.cwd))? != cwd {
            return Err(invalid("transcript id or working directory does not match"));
        }
        Ok(Self {
            model: header.model,
            messages,
        })
    }
}

pub struct Runtime {
    pub cwd: PathBuf,
    pub agent: Agent,
    pub recorder: SessionRecorder,
    pub used: u64,
    pub cost: f64,
}

impl Runtime {
    pub async fn build(
        config: &Config,
        registry: Arc<Registry>,
        cwd: PathBuf,
        id: &str,
        restored: Option<RestoredSession>,
    ) -> Result<Self> {
        if !cupel_coding_agent::project_trust::is_trusted(config.home.as_deref(), &cwd) {
            tracing::warn!(cwd = %&cwd.display(), "prompt hooks and sesitive model rows disabled: project is not trusted");
        }
        // The Plain capability set deliberately omits ask_user, no blocking from UI.
        let mut ingredients = bootstrap::load(
            &cwd,
            config.home.clone(),
            &registry,
            bootstrap::Frontend::Plain,
        )
        .await;
        for warning in &ingredients.warnings {
            tracing::warn!("{warning}");
        }
        let restored_model = restored.as_ref().map(|session| session.model.as_str());
        let (model, key) = startup::select_model(
            config.model.as_deref().or(restored_model),
            &ingredients.models,
            &ingredients.settings,
            config.home.as_deref(),
        )
        .map_err(invalid)?;
        let recorder = SessionRecorder::new(config.home.clone(), &cwd, id, &model.id);
        ingredients.bind_session(&recorder, &cwd);
        let preset = ingredients.settings.presets.get(DEFAULT_PRESET);
        let mut options = AgentOptions::new(model.clone(), registry);
        options.system_prompt = ingredients.system_prompt();
        options.thinking_level = startup::thinking_level(config.thinking, &model, preset);
        options.api_key = key;
        options.tools = ingredients.tools;
        options.hooks = Arc::new(ingredients.hooks);
        options.messages = restored.map(|session| session.messages).unwrap_or_default();
        let mut used = 0;
        let mut cost = 0.0;
        for message in &options.messages {
            account(message, &mut used, &mut cost);
        }
        Ok(Self {
            cwd,
            agent: Agent::new(options),
            recorder,
            used,
            cost,
        })
    }

    pub async fn prompt(
        &mut self,
        text: &str,
        cancel: &CancellationToken,
        notify: &Notify,
    ) -> Result<acp::PromptResponse> {
        // Do not drop a running hook future. Cancellation is checked after bounded preparation.
        self.recorder.before_prompt(text).await;
        if cancel.is_cancelled() {
            self.report_usage(notify)?;
            return Ok(acp::PromptResponse::new(acp::StopReason::Cancelled));
        }
        let mut events = self
            .agent
            .prompt_text(text)
            .map_err(|error| failure(error.to_string()))?;
        let mut mapper = Mapper::default();
        let mut output_error = None;
        let mut aborted = false;
        let mut ended = false;
        loop {
            tokio::select! {
                biased;
                () = cancel.cancelled(), if !aborted => {
                    aborted = true;
                    self.agent.abort();
                }
                event = events.next() => {
                    let Some(event) = event else { break };
                    if let AgentEvent::MessageEnd { message } = &event {
                        self.recorder.record(message);
                        account(message, &mut self.used, &mut self.cost);
                    }
                    if matches!(event, AgentEvent::AgentEnd { .. }) {
                        ended = true;
                        self.recorder.on_agent_end();
                    }
                    for update in mapper.event(&event) {
                        if output_error.is_none() && let Err(error) = notify(update) {
                            output_error = Some(error);
                            self.agent.abort();
                        }
                    }
                }
            }
        }
        self.agent.wait_for_idle().await;
        if let Some(error) = output_error {
            return Err(error);
        }
        if !ended {
            return Err(failure("agent event stream closed before AgentEnd"));
        }
        self.report_usage(notify)?;
        mapper.finish(cancel.is_cancelled())
    }

    fn report_usage(&self, notify: &Notify) -> Result<()> {
        notify(acp::SessionUpdate::UsageUpdate(
            acp::UsageUpdate::new(self.used, self.agent.context_window())
                .cost(acp::Cost::new(self.cost, "USD")),
        ))
    }
}

fn account(message: &AgentMessage, used: &mut u64, cost: &mut f64) {
    if let AgentMessage::Llm(cupel_core::types::Message::Assistant(assistant)) = message {
        let usage = &assistant.usage;
        let reported = usage
            .input
            .saturating_add(usage.output)
            .saturating_add(usage.cache_read)
            .saturating_add(usage.cache_write);
        if reported > 0 {
            *used = reported;
        }
        *cost += usage.cost.total;
    }
}

#[cfg(test)]
mod tests {
    use cupel_coding_agent::startup::ThinkingChoice;
    use cupel_core::types::ThinkingLevel;

    use super::*;

    #[tokio::test]
    async fn new_and_loaded_sessions_ignore_legacy_preset_prompts() {
        let root = std::env::temp_dir().join(format!(
            "cupel-acp-preset-{}-{}",
            std::process::id(),
            cupel_core::types::now_ms()
        ));
        let home = root.join("home");
        std::fs::create_dir_all(&home).unwrap();
        let cwd = root.join("project");
        std::fs::create_dir_all(&cwd).unwrap();
        let cwd = directory(&cwd).unwrap();
        let settings = |model: &str| {
            serde_json::json!({"model": {"default": {
                "provider": "anthropic", "model": model, "thinkingLevel": "low",
                "prompt": "LEGACY PRESET INSTRUCTIONS"
            }}})
            .to_string()
        };
        let settings_path = home.join("settings.json");
        std::fs::write(&settings_path, settings("claude-haiku-5-5")).unwrap();
        let guide = cwd.join("AGENTS.md");
        std::fs::write(&guide, "INITIAL PROJECT INSTRUCTIONS").unwrap();

        let registry = Arc::new(cupel_core::default_registry());
        let mut config = Config {
            home: Some(home),
            ..Config::default()
        };
        let mut runtime =
            Runtime::build(&config, Arc::clone(&registry), cwd.clone(), "saved", None)
                .await
                .unwrap();
        let state = runtime.agent.state();
        assert_eq!(state.model.id, "claude-haiku-5-5");
        assert_eq!(state.thinking_level, Some(ThinkingLevel::Low));
        assert!(state.system_prompt.contains("INITIAL PROJECT INSTRUCTIONS"));
        assert!(!state.system_prompt.contains("LEGACY PRESET INSTRUCTIONS"));
        let old_message = AgentMessage::user_text("earlier prompt");
        runtime.recorder.record(&old_message);
        drop(runtime);

        config.thinking = ThinkingChoice::Explicit(Some(ThinkingLevel::High));
        std::fs::write(&settings_path, settings("claude-sonnet-5")).unwrap();
        std::fs::write(&guide, "UPDATED PROJECT INSTRUCTIONS").unwrap();
        let restored = RestoredSession::load(config.home.as_deref(), &cwd, "saved").unwrap();
        let runtime = Runtime::build(&config, registry, cwd, "saved", Some(restored))
            .await
            .unwrap();
        let state = runtime.agent.state();
        assert_eq!(state.model.id, "claude-haiku-5-5", "transcript model wins");
        assert_eq!(state.thinking_level, Some(ThinkingLevel::High));
        assert_eq!(state.messages, vec![old_message]);
        assert!(state.system_prompt.contains("UPDATED PROJECT INSTRUCTIONS"));
        assert!(!state.system_prompt.contains("INITIAL PROJECT INSTRUCTIONS"));
        assert!(!state.system_prompt.contains("LEGACY PRESET INSTRUCTIONS"));
        std::fs::remove_dir_all(root).unwrap();
    }
}
