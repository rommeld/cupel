//! One session's agent, tools, transcript, and lifecycle hooks.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use agent_client_protocol::schema::v1 as acp;
use cupel_agent::{Agent, AgentEvent, AgentOptions};
use cupel_coding_agent::{bootstrap, session::SessionRecorder, settings::DEFAULT_PRESET, startup};
use cupel_core::provider::Registry;
use futures_util::StreamExt as _;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::events::{Mapper, replay};

pub type Notify = Arc<dyn Fn(acp::SessionUpdate) -> acp::Result<()> + Send + Sync>;

pub fn invalid(message: impl Into<String>) -> acp::Error {
    acp::Error::new(-32602, message)
}

pub fn failure(message: impl Into<String>) -> acp::Error {
    acp::Error::new(-32603, message)
}

pub fn directory(path: &Path) -> acp::Result<PathBuf> {
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

pub fn prompt_text(blocks: &[acp::ContentBlock]) -> acp::Result<String> {
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
        load: bool,
        notify: &Notify,
    ) -> acp::Result<Self> {
        let (restored_model, messages) = if load {
            if id.is_empty()
                || id.len() > 128
                || !id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            {
                return Err(invalid("invalid session id"));
            }
            let path = cupel_coding_agent::session::sessions_dir(config.home.as_deref(), &cwd)
                .ok_or_else(|| invalid("session persistence requires a Cupel home"))?
                .join(format!("{id}.jsonl"));
            let (header, messages) =
                cupel_coding_agent::session::load_transcript(&path).map_err(invalid)?;
            if header.session_id != id || directory(Path::new(&header.cwd))? != cwd {
                return Err(invalid("transcript id or working directory does not match"));
            }
            (Some(header.model), messages)
        } else {
            (None, Vec::new())
        };

        if !cupel_coding_agent::project_trust::is_trusted(config.home.as_deref(), &cwd) {
            tracing::warn!(cwd = %&cwd.display(), "prompt hooks and sesitive model rows disabled: project is not trusted");
        }
        // The Plain capability set deliberately omits ask_user: no blocking from UI.
        let ingredients = bootstrap::load(
            &cwd,
            config.home.clone(),
            &registry,
            bootstrap::Frontend::Plain,
        )
        .await;
        for warning in &ingredients.warnings {
            tracing::warn!("{warning}");
        }
        let (model, key) = startup::select_model(
            config.model.as_deref().or(restored_model.as_deref()),
            &ingredients.models,
            &ingredients.settings,
            config.home.as_deref(),
        )
        .map_err(invalid)?;
        let preset = ingredients.settings.presets.get(DEFAULT_PRESET);
        let mut options = AgentOptions::new(model.clone(), registry);
        options.system_prompt = cupel_coding_agent::system_prompt::with_preset_prompt(
            &ingredients.system_prompt,
            preset.and_then(|p| p.prompt.as_deref()),
        );
        options.thinking_level = startup::thinking_level(config.thinking, &model, preset);
        options.api_key = key;
        options.tools = ingredients.tools;
        options.hooks = Arc::new(ingredients.hooks);
        for message in &messages {
            for update in replay(message) {
                notify(update)?;
            }
        }
        let mut used = 0;
        let mut cost = 0.0;
        for message in &messages {
            account(message, &mut used, &mut cost);
        }
        options.messages = messages;
        let recorder = SessionRecorder::new(config.home.clone(), &cwd, id, &model.id);
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
    ) -> acp::Result<acp::PromptResponse> {
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

    fn report_usage(&self, notify: &Notify) -> acp::Result<()> {
        notify(acp::SessionUpdate::UsageUpdate(
            acp::UsageUpdate::new(self.used, self.agent.context_window())
                .cost(acp::Cost::new(self.cost, "USD")),
        ))
    }
}

fn account(message: &cupel_agent::AgentMessage, used: &mut u64, cost: &mut f64) {
    if let cupel_agent::AgentMessage::Llm(cupel_core::types::Message::Assistant(assistant)) =
        message
    {
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
