//! User-defined helpers, constructed only on their first call.
//! Each helper owns its prompt, tools and private provider history. Calls to
//! the same helper serialize; cancellation also covers callers waiting in line.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::hash::{DefaultHasher, Hash as _, Hasher as _};
use std::io::Write as _;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use futures_util::StreamExt as _;
use tokio_util::sync::CancellationToken;

use cupel_agent::types::BeforeToolCallResult;
use cupel_agent::{Agent, AgentEvent, AgentHooks, AgentMessage, AgentOptions, ToolExecutionMode};
use cupel_core::provider::Registry;
use cupel_core::types::{
    AssistantContent, AssistantMessage, Message, Model, StopReason, ToolCall, ToolResultMessage,
    Usage,
};

use crate::bootstrap::{SessionHooks, TOOL_SUMMARIES};
use crate::commands::PromptTemplate;
use crate::guard::BashGuard;
use crate::loop_killer::LoopKiller;
use crate::resources::ContextFile;
use crate::session::SessionRecorder;
use crate::settings::{AgentDefinition, Settings};
use crate::system_prompt::{build_system_prompt, with_preset_prompt};
use crate::truncate::{TruncationOptions, format_size, truncate_head};

/// A configured helper whose model exists in the catalog.
#[derive(Debug, Clone)]
pub struct AgentSpec {
    pub name: String,
    pub definition: AgentDefinition,
    pub model: Model,
}

/// Resolve model definitions and report missing credentials without refreshing them.
#[must_use]
pub fn resolve(
    settings: &Settings,
    models: &[Model],
    home: Option<&Path>,
) -> (Vec<AgentSpec>, Vec<String>) {
    let mut specs = Vec::new();
    let mut warnings = Vec::new();
    for (name, definition) in &settings.agents {
        let Some(model) = definition.preset.find_model(models) else {
            warnings.push(format!(
                "warning: ignoring agent {name}: unknown model {}/{} (/model lists them)",
                definition.preset.provider, definition.preset.model
            ));
            continue;
        };
        let provider = model.provider.as_str();
        let problem = match provider {
            "openai-codex" => (!crate::auth::has_credential(home, provider))
                .then(|| "not logged in to openai-codex (/login openai-codex)".to_string()),
            "amazon-bedrock" => (!crate::providers::has_aws_credentials())
                .then(|| "no AWS credentials for amazon-bedrock".to_string()),
            _ => (crate::providers::takes_api_key(models, provider)
                && crate::providers::resolve_api_key(provider, settings).is_none())
            .then(|| format!("no API key for {provider} (/provider {provider} <api-key>)")),
        };
        if let Some(problem) = problem {
            warnings.push(format!("warning: agent {name}: {problem}"));
        }
        specs.push(AgentSpec {
            name: name.clone(),
            definition: definition.clone(),
            model: model.clone(),
        });
    }
    (specs, warnings)
}

/// Eligibility does not depend on the optional fresh-session preference.
#[must_use]
pub fn team_member_problem(crew: &Crew, name: &str) -> Option<String> {
    match crew.spec(name) {
        None => Some(format!("no agent named {name} (/team lists them)")),
        Some(spec) if spec.definition.description().is_none() => Some(format!(
            "{name} has no description - add agents.{name}.description in settings.json, or call it yourself with /agent {name}"
        )),
        Some(_) => None,
    }
}

/// Validate explicit routes without granting membership or constructing helpers.
#[must_use]
pub fn routes(
    settings: &Settings,
    crew: &Crew,
    templates: &[PromptTemplate],
) -> (BTreeMap<String, String>, Vec<String>) {
    let mut routes = BTreeMap::new();
    let mut warnings = Vec::new();
    for (command, agent) in &settings.agent_commands {
        let sends_prompt = command == "review"
            || (!crate::commands::BUILTIN_COMMANDS
                .iter()
                .any(|builtin| builtin.name == command)
                && templates.iter().any(|template| &template.name == command));
        if !sends_prompt {
            warnings.push(format!("warning: ignoring agentCommands entry {command}: only /review and prompt templates can go to an agent"));
        } else if crew.spec(agent).is_none() {
            warnings.push(format!("warning: ignoring agentCommands entry {command}: no agent named {agent}; /{command} falls back to the lead"));
        } else {
            routes.insert(command.clone(), agent.clone());
        }
    }
    (routes, warnings)
}

/// Session ingredients shared with helpers, without constructing an Agent.
pub struct CrewSetup {
    pub cwd: PathBuf,
    pub home: Option<PathBuf>,
    pub registry: Arc<Registry>,
    pub settings: Settings,
    pub context_files: Vec<ContextFile>,
    pub session_id: String,
}

/// Usage belongs to the helper that made the provider request.
#[derive(Debug, Clone, Copy, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Spend {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cost: f64,
    pub calls: u32,
}

impl Spend {
    fn add_usage(&mut self, usage: &Usage) {
        self.input += usage.input;
        self.output += usage.output;
        self.cache_read += usage.cache_read;
        self.cost += usage.cost.total;
    }

    pub fn add(&mut self, other: &Self) {
        self.input += other.input;
        self.output += other.output;
        self.cache_read += other.cache_read;
        self.cost += other.cost;
        self.calls += other.calls;
    }
}

#[derive(Debug, Clone)]
pub struct Reply {
    pub name: String,
    pub answer: String,
    pub stopped_after: Option<u32>,
    pub tool_calls: usize,
    pub spend: Spend,
    pub full_answer: Option<PathBuf>,
    pub changes: Changes,
}

const FOR_LEAD: TruncationOptions = TruncationOptions {
    max_lines: Some(300),
    max_bytes: Some(12 * 1024),
};

impl Reply {
    #[must_use]
    pub fn for_lead(&self) -> String {
        let cut = truncate_head(&self.answer, FOR_LEAD);
        let mut text = if cut.content.trim().is_empty() {
            "(no answer)".into()
        } else {
            cut.content
        };
        if cut.truncated {
            let size = format_size(cut.total_bytes);
            text.push_str(&match &self.full_answer {
                Some(path) => format!(
                    "\n[answer cut, it has {size}: read {} for the rest]",
                    path.display()
                ),
                None => format!("\n[answer cut, it has {size}: the rest was not kept]"),
            });
        }
        if let Some(turns) = self.stopped_after {
            text.push_str(&format!(
                "\n[{} stopped after {turns} turns (maxTurns), before it finished]",
                self.name
            ));
        }
        text.push_str(&self.changes.to_string());
        text
    }
}

/// Observed checkout changes, not exclusive attribution to a helper.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Changes {
    ReadOnly,
    Unknown,
    Files(Vec<String>),
}

impl core::fmt::Display for Changes {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::ReadOnly => Ok(()),
            Self::Unknown => write!(
                f,
                "\n[changed files unknown: could not inspect git working tree]"
            ),
            Self::Files(files) if files.is_empty() => write!(f, "\n[no files changed]"),
            Self::Files(files) => {
                let names = truncate_head(
                    &files.join(", "),
                    TruncationOptions {
                        max_lines: Some(20),
                        max_bytes: Some(2048),
                    },
                );
                write!(
                    f,
                    "\n[files changed: {}{} - read them again before editing]",
                    names.content,
                    if names.truncated {
                        " (list cut; inspect git status for the rest)"
                    } else {
                        ""
                    }
                )
            }
        }
    }
}

pub const EXCHANGE_MARKER: &str = "[agent exchange]";

#[must_use]
pub fn exchange_note(task: &str, reply: &Reply) -> String {
    let task = truncate_head(
        task,
        TruncationOptions {
            max_lines: Some(30),
            max_bytes: Some(1024),
        },
    )
    .content;
    format!(
        "{EXCHANGE_MARKER} {name}\nThe user asked the agent {name} directly; you did not see it work. This is the exchange, so you know about it.\n\nTask:\n{task}\n\nAnswer:\n{answer}",
        name = reply.name,
        answer = reply.for_lead()
    )
}

#[must_use]
pub fn parse_exchange(text: &str) -> Option<(&str, &str)> {
    text.strip_prefix(EXCHANGE_MARKER)?
        .strip_prefix(' ')?
        .split_once('\n')
}

#[derive(Debug, thiserror::Error)]
pub enum CrewError {
    #[error("no agent named {0} (/team lists them)")]
    Unknown(String),
    #[error("{name} was stopped by the user{changes}")]
    Stopped { name: String, changes: Changes },
    #[error("{name} failed: {message}{changes}")]
    Failed {
        name: String,
        message: String,
        changes: Changes,
    },
}

struct HelperHooks {
    session: SessionHooks,
    max_turns: Option<NonZeroU32>,
    turns: AtomicU32,
}

#[async_trait::async_trait]
impl AgentHooks for HelperHooks {
    async fn api_key(&self, provider: &str) -> Option<String> {
        self.session.api_key(provider).await
    }

    async fn before_tool_call(
        &self,
        assistant: &AssistantMessage,
        tool_call: &ToolCall,
    ) -> Option<BeforeToolCallResult> {
        self.session.before_tool_call(assistant, tool_call).await
    }

    async fn should_stop_after_turn(
        &self,
        _message: &AssistantMessage,
        _tool_results: &[ToolResultMessage],
    ) -> bool {
        let turns = self.turns.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_turns.is_some_and(|max| turns >= max.get())
    }
}

struct Helper {
    agent: Agent,
    hooks: Arc<HelperHooks>,
    log: SessionRecorder,
}

/// Definitions, membership (owned by the frontend), and construction are distinct.
pub struct Crew {
    specs: BTreeMap<String, AgentSpec>,
    setup: CrewSetup,
    helpers: Mutex<BTreeMap<String, Arc<tokio::sync::Mutex<Helper>>>>,
    running: Mutex<BTreeMap<u64, (String, CancellationToken)>>,
    next_call: AtomicU64,
    spend: Mutex<BTreeMap<String, Spend>>,
    keys: Mutex<HashMap<String, String>>,
}

impl Crew {
    #[must_use]
    pub fn new(specs: Vec<AgentSpec>, setup: CrewSetup) -> Self {
        Self {
            specs: specs
                .into_iter()
                .map(|spec| (spec.name.clone(), spec))
                .collect(),
            setup,
            helpers: Mutex::new(BTreeMap::new()),
            running: Mutex::new(BTreeMap::new()),
            next_call: AtomicU64::new(0),
            spend: Mutex::new(BTreeMap::new()),
            keys: Mutex::new(HashMap::new()),
        }
    }

    #[must_use]
    pub fn spec(&self, name: &str) -> Option<&AgentSpec> {
        self.specs.get(name)
    }

    pub fn specs(&self) -> impl Iterator<Item = &AgentSpec> {
        self.specs.values()
    }

    #[must_use]
    pub fn is_built(&self, name: &str) -> bool {
        self.helpers
            .lock()
            .expect("crew lock poisoned")
            .contains_key(name)
    }

    pub fn remember_key(&self, provider: &str, key: &str) {
        self.keys
            .lock()
            .expect("crew lock poisoned")
            .insert(provider.into(), key.into());
    }

    pub fn remember_keys(&self, keys: &HashMap<String, String>) {
        self.keys
            .lock()
            .expect("crew lock poisoned")
            .extend(keys.clone());
    }

    #[must_use]
    pub fn spend_of(&self, name: &str) -> Spend {
        self.spend
            .lock()
            .expect("crew lock poisoned")
            .get(name)
            .copied()
            .unwrap_or_default()
    }

    #[must_use]
    pub fn spend_snapshot(&self) -> BTreeMap<String, Spend> {
        self.spend.lock().expect("crew lock poisoned").clone()
    }

    pub fn restore_spend(&self, spend: BTreeMap<String, Spend>) {
        *self.spend.lock().expect("crew lock poisoned") = spend;
    }

    #[must_use]
    pub fn total_spend(&self) -> Spend {
        self.spend
            .lock()
            .expect("crew lock poisoned")
            .values()
            .fold(Spend::default(), |mut total, spend| {
                total.add(spend);
                total
            })
    }

    #[must_use]
    pub fn running_names(&self) -> Vec<String> {
        self.running
            .lock()
            .expect("crew lock poisoned")
            .values()
            .map(|(name, _)| name.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    /// Already-cancelled calls do not consume a second Esc intended for the lead.
    pub fn stop_running(&self) -> Vec<String> {
        self.running
            .lock()
            .expect("crew lock poisoned")
            .values()
            .filter_map(|(name, stop)| {
                if stop.is_cancelled() {
                    return None;
                }
                stop.cancel();
                Some(name.clone())
            })
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    pub async fn wait_for_idle(&self) {
        let helpers = self
            .helpers
            .lock()
            .expect("crew lock poisoned")
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for helper in helpers {
            helper.lock().await.agent.wait_for_idle().await;
        }
    }

    pub async fn run(
        &self,
        name: &str,
        task: &str,
        cancel: CancellationToken,
        progress: &(dyn Fn(String) + Send + Sync),
    ) -> Result<Reply, CrewError> {
        let spec = self
            .specs
            .get(name)
            .ok_or_else(|| CrewError::Unknown(name.into()))?;
        let stop = cancel.child_token();
        let id = self.next_call.fetch_add(1, Ordering::Relaxed);
        self.running
            .lock()
            .expect("crew lock poisoned")
            .insert(id, (name.into(), stop.clone()));
        let mut on_duty = OnDuty {
            running: &self.running,
            id,
            agent_stop: None,
        };
        let stopped = || CrewError::Stopped {
            name: name.into(),
            changes: Changes::ReadOnly,
        };
        if stop.is_cancelled() {
            return Err(stopped());
        }
        let helper = self.helper(spec);
        let mut helper = tokio::select! {
            biased;
            () = stop.cancelled() => return Err(stopped()),
            helper = helper.lock() => helper,
        };
        // A dropped caller cancels its inner run; never reuse its history mid-run.
        tokio::select! {
            biased;
            () = stop.cancelled() => return Err(stopped()),
            () = helper.agent.wait_for_idle() => {}
        }
        if stop.is_cancelled() {
            return Err(stopped());
        }
        if !spec.definition.remembers() {
            helper.agent.reset();
        }
        helper.hooks.turns.store(0, Ordering::SeqCst);
        helper
            .agent
            .set_api_key(self.key_for(spec.model.provider.as_str()));
        let before = if spec.definition.writes() {
            Some(snapshot(&self.setup.cwd).await)
        } else {
            None
        };
        if stop.is_cancelled() {
            return Err(stopped());
        }
        let failed = |message: String| CrewError::Failed {
            name: name.into(),
            message,
            changes: Changes::ReadOnly,
        };
        let mut events = helper
            .agent
            .prompt_text(task)
            .map_err(|error| failed(error.to_string()))?;
        on_duty.agent_stop = helper.agent.cancel_token();
        let mut lines = Vec::new();
        let mut spend = Spend {
            calls: 1,
            ..Spend::default()
        };
        let mut last = None;
        let mut stopping = false;
        let mut ended = false;
        loop {
            tokio::select! {
                () = stop.cancelled(), if !stopping => {
                    stopping = true;
                    helper.agent.abort();
                }
                event = events.next() => match event {
                    None => break,
                    Some(AgentEvent::AgentEnd { .. }) => { ended = true; break; }
                    Some(AgentEvent::MessageEnd { message }) => {
                        helper.log.record(&message);
                        if let AgentMessage::Llm(Message::Assistant(assistant)) = message {
                            spend.add_usage(&assistant.usage);
                            let known = lines.len();
                            for content in &assistant.content {
                                if let AssistantContent::ToolCall(call) = content {
                                    lines.push(helper.agent.describe_tool_call(&call.name, &call.arguments));
                                }
                            }
                            if lines.len() > known { progress(lines.join("\n")); }
                            last = Some(assistant);
                        }
                    }
                    Some(_) => {}
                }
            }
        }
        helper.agent.wait_for_idle().await;
        self.spend
            .lock()
            .expect("crew lock poisoned")
            .entry(name.into())
            .or_default()
            .add(&spend);
        let changes = match before {
            None => Changes::ReadOnly,
            Some(before) => match (before, snapshot(&self.setup.cwd).await) {
                (Some(before), Some(after)) if before.head == after.head => {
                    Changes::Files(changed(&before.files, &after.files))
                }
                _ => Changes::Unknown,
            },
        };
        let failed = |message: String| CrewError::Failed {
            name: name.into(),
            message,
            changes: changes.clone(),
        };
        let stopped = || CrewError::Stopped {
            name: name.into(),
            changes: changes.clone(),
        };
        if !ended {
            return Err(failed("helper event stream closed before AgentEnd".into()));
        }
        let Some(last) = last else {
            return Err(if stop.is_cancelled() {
                stopped()
            } else {
                failed("the model sent no reply".into())
            });
        };
        if stop.is_cancelled() && last.stop_reason != StopReason::Stop {
            return Err(stopped());
        }
        if matches!(last.stop_reason, StopReason::Error | StopReason::Aborted) {
            return Err(failed(
                last.error_message.unwrap_or_else(|| "unknown error".into()),
            ));
        }
        let answer = answer_text(&last);
        let full_answer = if truncate_head(&answer, FOR_LEAD).truncated {
            self.keep_answer(name, &answer)
        } else {
            None
        };
        Ok(Reply {
            name: name.into(),
            answer,
            stopped_after: (last.stop_reason == StopReason::ToolUse)
                .then(|| helper.hooks.turns.load(Ordering::SeqCst)),
            tool_calls: lines.len(),
            spend,
            full_answer,
            changes,
        })
    }

    fn log_dir(&self) -> Option<PathBuf> {
        crate::session::sessions_dir(self.setup.home.as_deref(), &self.setup.cwd)
            .map(|dir| dir.join(&self.setup.session_id))
    }

    fn keep_answer(&self, name: &str, answer: &str) -> Option<PathBuf> {
        let dir = self.log_dir()?;
        std::fs::create_dir_all(&dir).ok()?;
        for _ in 0..10 {
            let id = self.next_call.fetch_add(1, Ordering::Relaxed);
            let path = dir.join(format!("{name}-{}-{id}.md", cupel_core::types::now_ms()));
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut file) => match file.write_all(answer.as_bytes()) {
                    Ok(()) => return Some(path),
                    Err(error) => {
                        tracing::warn!(path = %path.display(), "cannot keep helper answer: {error}");
                        return None;
                    }
                },
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => {
                    tracing::warn!(path = %path.display(), "cannot keep helper answer: {error}");
                    return None;
                }
            }
        }
        None
    }

    fn key_for(&self, provider: &str) -> Option<String> {
        self.keys
            .lock()
            .expect("crew lock poisoned")
            .get(provider)
            .cloned()
            .or_else(|| crate::providers::resolve_api_key(provider, &self.setup.settings))
    }

    fn helper(&self, spec: &AgentSpec) -> Arc<tokio::sync::Mutex<Helper>> {
        let mut helpers = self.helpers.lock().expect("crew lock poisoned");
        Arc::clone(
            helpers
                .entry(spec.name.clone())
                .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(self.build(spec)))),
        )
    }

    fn build(&self, spec: &AgentSpec) -> Helper {
        let setup = &self.setup;
        let allows = |tool: &str| {
            spec.definition
                .tools()
                .iter()
                .any(|name| name.as_str() == tool)
        };
        let tools = crate::bootstrap::coding_tools(&setup.cwd)
            .into_iter()
            .filter(|tool| allows(tool.name()))
            .collect();
        let summaries = TOOL_SUMMARIES
            .iter()
            .copied()
            .filter(|(name, _)| allows(name))
            .collect::<Vec<_>>();
        let base = build_system_prompt(&setup.cwd, &summaries, &setup.context_files);
        let note = format!(
            "You are {}, a helper agent. The lead agent or the user gives you one task at a time. Work on it with your tools, then end with a complete, concise answer, with file paths and line numbers where they help.",
            spec.name
        );
        let prompt = with_preset_prompt(&base, Some(&note));
        let (guard, _) = BashGuard::from_config(setup.home.as_deref(), &setup.cwd);
        let hooks = Arc::new(HelperHooks {
            session: SessionHooks::new(
                guard,
                LoopKiller::new(setup.settings.loop_killer_max_repeats()),
                setup.home.clone(),
            ),
            max_turns: spec.definition.max_turns,
            turns: AtomicU32::new(0),
        });
        let log = SessionRecorder::new(
            setup.home.clone(),
            &setup.cwd,
            &format!("{}/{}", setup.session_id, spec.name),
            &spec.model.id,
        );
        let history = self
            .log_dir()
            .map(|dir| dir.join(format!("{}.jsonl", spec.name)))
            .filter(|path| spec.definition.remembers() && path.exists())
            .and_then(|path| match crate::session::load_transcript(&path) {
                Ok((_, messages)) => Some(messages),
                Err(error) => {
                    tracing::warn!(path = %path.display(), "helper starts fresh: {error}");
                    None
                }
            })
            .unwrap_or_default();
        let mut options = AgentOptions::new(spec.model.clone(), Arc::clone(&setup.registry));
        options.system_prompt =
            with_preset_prompt(&prompt, spec.definition.preset.prompt.as_deref());
        options.tools = tools;
        options.hooks = Arc::clone(&hooks) as Arc<dyn AgentHooks>;
        options.thinking_level = spec.definition.preset.thinking_level.level();
        options.tool_execution = ToolExecutionMode::Parallel;
        options.session_id = Some(format!("{}/{}", setup.session_id, spec.name));
        options.messages = history;
        Helper {
            agent: Agent::new(options),
            hooks,
            log,
        }
    }
}

struct OnDuty<'a> {
    running: &'a Mutex<BTreeMap<u64, (String, CancellationToken)>>,
    id: u64,
    agent_stop: Option<CancellationToken>,
}

impl Drop for OnDuty<'_> {
    fn drop(&mut self) {
        if let Some(stop) = &self.agent_stop {
            stop.cancel();
        }
        if let Ok(mut running) = self.running.lock() {
            running.remove(&self.id);
        }
    }
}

fn answer_text(message: &AssistantMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|content| match content {
            AssistantContent::Text(text) => Some(text.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

struct WorkingTree {
    head: Option<String>,
    files: BTreeMap<String, Option<u64>>,
}

fn dirty_files(cwd: &Path) -> Option<WorkingTree> {
    let head = git(cwd, &["rev-parse", "--verify", "HEAD"]);
    let root = git(cwd, &["rev-parse", "--show-toplevel"])?;
    let root = PathBuf::from(root.strip_suffix('\n').unwrap_or(&root));
    let status = git(
        cwd,
        &["status", "--porcelain=v1", "-z", "--untracked-files=all"],
    )?;
    let mut entries = status.split('\0');
    let mut files = BTreeMap::new();
    while let Some(entry) = entries.next() {
        let (Some(code), Some(path)) = (entry.get(..2), entry.get(3..)) else {
            continue;
        };
        if code.contains(['R', 'C']) {
            let old = entries.next()?;
            if code.contains('R') {
                files.insert(root.join(old).to_str()?.to_string(), None);
            }
        }
        let full = root.join(path);
        let content = match std::fs::symlink_metadata(&full) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(_) => return None,
            Ok(metadata) => {
                let mut hasher = DefaultHasher::new();
                if metadata.file_type().is_symlink() {
                    std::fs::read_link(&full).ok()?.hash(&mut hasher);
                } else if metadata.is_file() {
                    std::fs::read(&full).ok()?.hash(&mut hasher);
                } else {
                    return None;
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt as _;
                    metadata.permissions().mode().hash(&mut hasher);
                }
                Some(hasher.finish())
            }
        };
        files.insert(full.to_str()?.to_string(), content);
    }
    Some(WorkingTree { head, files })
}

async fn snapshot(cwd: &Path) -> Option<WorkingTree> {
    let cwd = cwd.to_path_buf();
    tokio::task::spawn_blocking(move || dirty_files(&cwd))
        .await
        .ok()
        .flatten()
}

fn changed(
    before: &BTreeMap<String, Option<u64>>,
    after: &BTreeMap<String, Option<u64>>,
) -> Vec<String> {
    before
        .keys()
        .chain(after.keys())
        .filter(|path| before.get(*path) != after.get(*path))
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn git(cwd: &Path, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8(output.stdout).ok())
        .flatten()
}

#[cfg(test)]
mod tests {
    use crate::crew::*;
    use cupel_core::event_stream::{AssistantMessageStream, assistant_message_channel};
    use cupel_core::provider::Provider;
    use cupel_core::types::{
        Api, Context, InputModality, ModelCost, StreamOptions, TextContent, now_ms,
    };
    use std::time::Duration;

    #[derive(Clone, Copy)]
    enum Step {
        Say,
        Read,
        Fail,
        Hang,
        Long,
        WriteFail,
        WriteHang,
    }

    struct Mock {
        step: Step,
        contexts: Mutex<Vec<Context>>,
        signals: Mutex<Vec<CancellationToken>>,
    }

    impl Provider for Mock {
        fn api(&self) -> &str {
            "mock"
        }

        fn stream(
            &self,
            model: &Model,
            context: Context,
            options: StreamOptions,
        ) -> AssistantMessageStream {
            let call = {
                let mut contexts = self.contexts.lock().unwrap();
                contexts.push(context);
                contexts.len()
            };
            let signal = options.signal.unwrap_or_default();
            self.signals.lock().unwrap().push(signal.clone());
            let (stream, sink) = assistant_message_channel();
            let step = self.step;
            let model = model.clone();
            tokio::spawn(async move {
                let _ = sink.start();
                let (content, stop) = match step {
                    Step::Long => (
                        vec![AssistantContent::Text(TextContent::plain(
                            "long line\n".repeat(400),
                        ))],
                        StopReason::Stop,
                    ),
                    Step::WriteFail | Step::WriteHang if call == 1 => (
                        vec![AssistantContent::ToolCall(ToolCall {
                            id: "write-1".into(),
                            name: "bash".into(),
                            arguments: serde_json::json!({"command":"printf changed > edited.txt"}),
                        })],
                        StopReason::ToolUse,
                    ),
                    Step::WriteFail | Step::Fail => (Vec::new(), StopReason::Error),
                    Step::WriteHang | Step::Hang => {
                        signal.cancelled().await;
                        (Vec::new(), StopReason::Aborted)
                    }
                    Step::Say => (
                        vec![AssistantContent::Text(TextContent::plain("answer"))],
                        StopReason::Stop,
                    ),
                    Step::Read => (
                        vec![AssistantContent::ToolCall(ToolCall {
                            id: "read-1".into(),
                            name: "read".into(),
                            arguments: serde_json::json!({"path":"note.md"}),
                        })],
                        StopReason::ToolUse,
                    ),
                };
                let message = AssistantMessage {
                    content,
                    api: model.api,
                    provider: model.provider,
                    model: model.id,
                    response_model: None,
                    response_id: None,
                    usage: Usage {
                        input: 10,
                        output: 5,
                        ..Usage::default()
                    },
                    stop_reason: stop,
                    error_message: matches!(step, Step::Fail | Step::WriteFail)
                        .then(|| "no credit".into()),
                    error_kind: None,
                    timestamp: now_ms(),
                };
                if matches!(stop, StopReason::Error | StopReason::Aborted) {
                    let _ = sink.error(stop, message);
                } else {
                    let _ = sink.done(stop, message);
                }
            });
            stream
        }
    }

    fn model() -> Model {
        Model {
            id: "mock-model".into(),
            name: "Mock".into(),
            api: Api::from("mock"),
            provider: cupel_core::types::Provider::from("mock"),
            base_url: String::new(),
            reasoning: false,
            thinking_level_map: None,
            input: vec![InputModality::Text],
            cost: ModelCost::default(),
            context_window: 100_000,
            max_context_window: None,
            max_tokens: 4096,
            headers: None,
            compat: None,
        }
    }

    fn crew(step: Step, extra: serde_json::Value) -> (Arc<Crew>, Arc<Mock>) {
        let mock = Arc::new(Mock {
            step,
            contexts: Mutex::new(Vec::new()),
            signals: Mutex::new(Vec::new()),
        });
        let mut registry = Registry::new();
        registry.register(Arc::clone(&mock) as Arc<dyn Provider>);
        let mut value =
            serde_json::json!({"provider":"mock","model":"mock-model","thinkingLevel":"off"});
        value
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        let mut settings = Settings::default();
        settings
            .agents
            .insert("scout".into(), serde_json::from_value(value).unwrap());
        let (specs, _) = resolve(&settings, &[model()], None);
        let cwd = std::env::temp_dir().join(format!(
            "cupel-crew-m2-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&cwd).unwrap();
        let cwd = std::fs::canonicalize(cwd).unwrap();
        std::fs::write(cwd.join("note.md"), "note").unwrap();
        (
            Arc::new(Crew::new(
                specs,
                CrewSetup {
                    cwd,
                    home: None,
                    registry: Arc::new(registry),
                    settings,
                    context_files: Vec::new(),
                    session_id: "test".into(),
                },
            )),
            mock,
        )
    }

    async fn started(mock: &Mock, count: usize) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while mock.contexts.lock().unwrap().len() < count {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
    }

    fn spawn(crew: &Arc<Crew>) -> tokio::task::JoinHandle<Result<Reply, CrewError>> {
        let crew = Arc::clone(crew);
        tokio::spawn(async move {
            crew.run("scout", "task", CancellationToken::new(), &|_| {})
                .await
        })
    }

    #[tokio::test]
    async fn lazy_private_history_and_tools() {
        for remember in [true, false] {
            let (crew, mock) = crew(
                Step::Say,
                serde_json::json!({"remember":remember,"tools":["read"],"prompt":"Report only."}),
            );
            assert!(!crew.is_built("scout"));
            assert!(mock.contexts.lock().unwrap().is_empty());
            for _ in 0..2 {
                assert_eq!(
                    crew.run("scout", "task", CancellationToken::new(), &|_| {})
                        .await
                        .unwrap()
                        .answer,
                    "answer"
                );
            }
            let contexts = mock.contexts.lock().unwrap();
            assert_eq!(contexts[1].messages.len(), if remember { 3 } else { 1 });
            let prompt = contexts[0].system_prompt.as_deref().unwrap();
            assert!(prompt.contains("You are scout, a helper agent."));
            assert!(prompt.ends_with("Report only."));
            assert!(!prompt.contains("- bash:"));
            let tools = contexts[0].tools.as_ref().unwrap();
            assert_eq!(tools.len(), 1);
            assert_eq!(tools[0].name, "read");
            assert_eq!(crew.spend_of("scout").input, 20);
            assert_eq!(crew.total_spend().calls, 2);
        }
    }

    #[tokio::test]
    async fn limits_and_errors_are_accounted_for() {
        let (crew, mock) = crew(Step::Read, serde_json::json!({"maxTurns":2}));
        let reply = crew
            .run("scout", "task", CancellationToken::new(), &|_| {})
            .await
            .unwrap();
        assert_eq!(reply.stopped_after, Some(2));
        assert_eq!(reply.tool_calls, 2);
        assert_eq!(mock.contexts.lock().unwrap().len(), 2);
        let (crew, _) = self::crew(Step::Fail, serde_json::json!({}));
        let error = crew
            .run("scout", "task", CancellationToken::new(), &|_| {})
            .await
            .unwrap_err();
        assert_eq!(error.to_string(), "scout failed: no credit");
        assert_eq!(crew.spend_of("scout").input, 10);
        assert!(matches!(
            crew.run("missing", "task", CancellationToken::new(), &|_| {})
                .await,
            Err(CrewError::Unknown(_))
        ));
    }

    #[tokio::test]
    async fn precancel_does_not_construct_or_request() {
        let (crew, mock) = crew(Step::Say, serde_json::json!({}));
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert!(matches!(
            crew.run("scout", "task", cancel, &|_| {}).await,
            Err(CrewError::Stopped { .. })
        ));
        assert!(!crew.is_built("scout"));
        assert!(mock.contexts.lock().unwrap().is_empty());
        assert!(crew.running_names().is_empty());
    }

    #[tokio::test]
    async fn queued_calls_are_registered_individually_and_cancellable() {
        let (crew, mock) = crew(Step::Hang, serde_json::json!({}));
        let first = spawn(&crew);
        started(&mock, 1).await;
        let second = spawn(&crew);
        tokio::time::timeout(Duration::from_secs(5), async {
            while crew.running.lock().unwrap().len() < 2 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(crew.running_names(), ["scout"]);
        assert_eq!(crew.stop_running(), ["scout"]);
        assert!(crew.stop_running().is_empty());
        for call in [first, second] {
            assert!(matches!(
                tokio::time::timeout(Duration::from_secs(5), call)
                    .await
                    .unwrap()
                    .unwrap(),
                Err(CrewError::Stopped { .. })
            ));
        }
        assert!(crew.running_names().is_empty());
        assert_eq!(mock.contexts.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn dropped_caller_cancels_inner_agent_before_reuse() {
        let (crew, mock) = crew(Step::Hang, serde_json::json!({}));
        let call = spawn(&crew);
        started(&mock, 1).await;
        call.abort();
        assert!(call.await.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(5), crew.wait_for_idle())
            .await
            .unwrap();
        assert!(mock.signals.lock().unwrap()[0].is_cancelled());
        assert!(crew.running_names().is_empty());
        let next = spawn(&crew);
        started(&mock, 2).await;
        crew.stop_running();
        assert!(matches!(
            next.await.unwrap(),
            Err(CrewError::Stopped { .. })
        ));
    }

    #[tokio::test]
    async fn long_answers_and_tasks_are_bounded_and_saved_without_overwrite() {
        let (mut crew, _) = crew(Step::Long, serde_json::json!({}));
        let setup = &mut Arc::get_mut(&mut crew).unwrap().setup;
        setup.home = Some(setup.cwd.join("home"));
        let reply = crew
            .run("scout", "task", CancellationToken::new(), &|_| {})
            .await
            .unwrap();
        let path = reply.full_answer.as_ref().unwrap();
        assert_eq!(std::fs::read_to_string(path).unwrap(), reply.answer);
        let second = crew
            .run("scout", "again", CancellationToken::new(), &|_| {})
            .await
            .unwrap();
        assert_ne!(second.full_answer, reply.full_answer);
        assert!(reply.for_lead().contains("answer cut"));
        let note = exchange_note(&"task".repeat(40_000), &reply);
        assert!(note.len() < 15 * 1024);
        assert_eq!(parse_exchange(&note).unwrap().0, "scout");
        assert!(parse_exchange("ordinary message").is_none());
    }

    #[tokio::test]
    async fn transcripts_resume_only_at_the_first_call() {
        let (mut first, mock) = crew(Step::Say, serde_json::json!({}));
        let setup = &mut Arc::get_mut(&mut first).unwrap().setup;
        setup.home = Some(setup.cwd.join("home"));
        assert!(!first.log_dir().unwrap().exists());
        first
            .run("scout", "first", CancellationToken::new(), &|_| {})
            .await
            .unwrap();
        let setup = &first.setup;
        let resumed = Crew::new(
            first.specs().cloned().collect(),
            CrewSetup {
                cwd: setup.cwd.clone(),
                home: setup.home.clone(),
                registry: Arc::clone(&setup.registry),
                settings: setup.settings.clone(),
                context_files: setup.context_files.clone(),
                session_id: setup.session_id.clone(),
            },
        );
        assert!(!resumed.is_built("scout"));
        resumed
            .run("scout", "second", CancellationToken::new(), &|_| {})
            .await
            .unwrap();
        let path = resumed.log_dir().unwrap().join("scout.jsonl");
        assert_eq!(crate::session::load_transcript(&path).unwrap().1.len(), 4);
        assert_eq!(mock.contexts.lock().unwrap()[1].messages.len(), 3);
    }

    #[tokio::test]
    async fn failures_and_stops_still_report_partial_edits() {
        for step in [Step::WriteFail, Step::WriteHang] {
            let (crew, mock) = crew(step, serde_json::json!({"tools":["bash"]}));
            assert!(git(&crew.setup.cwd, &["init", "-q"]).is_some());
            let call = spawn(&crew);
            started(&mock, 2).await;
            if matches!(step, Step::WriteHang) {
                crew.stop_running();
            }
            let error = tokio::time::timeout(Duration::from_secs(5), call)
                .await
                .unwrap()
                .unwrap()
                .unwrap_err();
            let changes = match error {
                CrewError::Failed { changes, .. } | CrewError::Stopped { changes, .. } => changes,
                error @ CrewError::Unknown(_) => panic!("unexpected {error}"),
            };
            let Changes::Files(paths) = changes else {
                panic!("missing changed files");
            };
            assert!(
                paths.contains(
                    &crew
                        .setup
                        .cwd
                        .join("edited.txt")
                        .to_string_lossy()
                        .into_owned()
                )
            );
        }
    }

    #[test]
    fn snapshots_include_renames_and_absolute_paths() {
        let (crew, _) = crew(Step::Say, serde_json::json!({}));
        let cwd = &crew.setup.cwd;
        git(cwd, &["init", "-q"]).unwrap();
        git(cwd, &["add", "note.md"]).unwrap();
        git(
            cwd,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-qm",
                "base",
            ],
        )
        .unwrap();
        let before = dirty_files(cwd).unwrap();
        git(cwd, &["mv", "note.md", "renamed.md"]).unwrap();
        let after = dirty_files(cwd).unwrap();
        let paths = changed(&before.files, &after.files);
        assert!(paths.contains(&cwd.join("note.md").to_string_lossy().into_owned()));
        assert!(paths.contains(&cwd.join("renamed.md").to_string_lossy().into_owned()));
    }
}
