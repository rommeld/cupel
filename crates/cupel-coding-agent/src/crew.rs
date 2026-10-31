//! User defined helpers, constructed only on their first call.
//! Each helper owns its prompt, tools and private provider history. Calls to
//! the same helper serialize, cancellation also covers callers waiting in line.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::hash::{DefaultHasher, Hash as _, Hasher as _};
use std::io::Write as _;
use std::num::NonZeroU32;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

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
use crate::roles::{AccessFilter, RoleIndex, RoleSelection};
use crate::session::SessionRecorder;
use crate::settings::{AgentDefinition, Settings};
use crate::system_prompt::{build_system_prompt, with_preset_prompt};
use crate::team_conversation::{Participant, TaskPhase, TeamRuntime};
use crate::team_runtime::ConversationHooks;
use crate::team_scheduler::{
    AccessScope, CallContext, CallId, CallScope, RunContext, TaskState, TaskVisibility, TeamLimits,
    TeamScheduler,
};
use crate::tools::checkout_write::{CheckoutWriteTool, WriteGate};
use crate::tools::team_conversation::CONVERSATION_TOOL_NAMES;
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
        if definition.description().is_none() {
            warnings.push(format!("warning: agent {name} is manual-only: add agents.{name}.description in settings.json to use it on a team; /agent {name} and agentCommands remain available"));
        }
        specs.push(AgentSpec {
            name: name.clone(),
            definition: definition.clone(),
            model: model.clone(),
        });
    }
    (specs, warnings)
}

/// Eligibility does not depend on the optional fresh session preference.
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
    #[serde(default)]
    pub unknown_usage: bool,
}

impl Spend {
    pub fn add_usage(&mut self, usage: &Usage) {
        self.unknown_usage |=
            usage.input + usage.output + usage.cache_read + usage.cache_write == 0;
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
        self.unknown_usage |= other.unknown_usage;
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct UsageCheckpoint {
    pub session: String,
    pub through: Option<CallId>,
    pub spend: BTreeMap<String, Spend>,
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
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Changes {
    ReadOnly,
    Unknown,
    Files(Vec<String>),
    Uncertain(Vec<String>),
}

impl Changes {
    #[must_use]
    pub fn merge(&self, other: &Self) -> Self {
        let files = [self, other]
            .into_iter()
            .flat_map(|changes| match changes {
                Self::Files(files) | Self::Uncertain(files) => files.as_slice(),
                Self::ReadOnly | Self::Unknown => &[],
            })
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        if matches!(self, Self::Unknown | Self::Uncertain(_))
            || matches!(other, Self::Unknown | Self::Uncertain(_))
        {
            Self::Uncertain(files)
        } else if matches!((self, other), (Self::ReadOnly, Self::ReadOnly)) {
            Self::ReadOnly
        } else {
            Self::Files(files)
        }
    }
}

impl core::fmt::Display for Changes {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::ReadOnly => Ok(()),
            Self::Uncertain(files) => write!(
                f,
                "{}\n[additional changed files unknown]",
                Self::Files(files.clone())
            ),
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
    context: Arc<CallContext>,
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

#[derive(Default)]
struct HelperSlot {
    value: tokio::sync::Mutex<Option<Helper>>,
    built: AtomicBool,
}

/// Definitions, membership (owned by the frontend), and construction are distinct.
pub struct Crew {
    specs: BTreeMap<String, AgentSpec>,
    setup: CrewSetup,
    team: Mutex<Option<Arc<TeamRuntime>>>,
    owner: Mutex<Weak<Crew>>,
    scheduler: Arc<TeamScheduler>,
    lead_context: Arc<CallContext>,
    write_gate: Mutex<WriteGate>,
    checkpoint_through: Mutex<Option<CallId>>,
    recovering: tokio::sync::Mutex<bool>,
    roles: RoleIndex,
    helpers: Mutex<BTreeMap<String, Arc<HelperSlot>>>,
    running: Mutex<BTreeMap<u64, (String, CancellationToken)>>,
    next_call: AtomicU64,
    spend: Mutex<BTreeMap<String, Spend>>,
    keys: Mutex<HashMap<String, String>>,
}

struct ScopedRequest {
    name: String,
    task: String,
    parent: Option<CallScope>,
    access: AccessScope,
    cancel: CancellationToken,
    shared: Option<Arc<TeamRuntime>>,
    selection: Option<(RoleSelection, AccessFilter)>,
}

impl Crew {
    #[must_use]
    pub fn new(specs: Vec<AgentSpec>, setup: CrewSetup) -> Self {
        let scheduler = TeamScheduler::new(TeamLimits::default());
        let roles = RoleIndex::new(specs.iter());
        scheduler.set_cwd(setup.cwd.clone());
        Self {
            specs: specs
                .into_iter()
                .map(|spec| (spec.name.clone(), spec))
                .collect(),
            setup,
            team: Mutex::new(None),
            owner: Mutex::new(Weak::new()),
            scheduler,
            lead_context: Arc::new(CallContext::default()),
            write_gate: Mutex::new(Arc::new(tokio::sync::Mutex::new(()))),
            checkpoint_through: Mutex::new(None),
            recovering: tokio::sync::Mutex::new(false),
            roles,
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
    #[must_use]
    pub fn roles(&self) -> &RoleIndex {
        &self.roles
    }

    fn validate_selection(
        &self,
        selection: &RoleSelection,
        filter: AccessFilter,
        parent: Option<&CallScope>,
    ) -> Result<RoleSelection, String> {
        let mut members = self.members();
        if let Some(parent) = parent {
            members.retain(|name| parent.can_call(name));
        }
        let current = self
            .roles
            .resolve(selection.role, &members, filter)
            .selection(selection.role)?;
        if current.agent != selection.agent {
            return Err("discovered target changed while queued; select again".into());
        }
        Ok(current)
    }

    pub fn set_team_runtime(self: &Arc<Self>, team: Arc<TeamRuntime>) {
        self.scheduler.attach(Arc::clone(&team));
        *self.owner.lock().expect("crew owner poisoned") = Arc::downgrade(self);
        *self.team.lock().expect("crew team lock poisoned") = Some(team);
    }

    #[must_use]
    pub fn with_team_limits(mut self, limits: TeamLimits) -> Self {
        self.scheduler = TeamScheduler::new(limits);
        self.scheduler.set_cwd(self.setup.cwd.clone());
        self
    }
    #[must_use]
    pub fn scheduler(&self) -> Arc<TeamScheduler> {
        Arc::clone(&self.scheduler)
    }
    #[must_use]
    pub fn lead_context(&self) -> Arc<CallContext> {
        Arc::clone(&self.lead_context)
    }
    #[must_use]
    pub fn write_gate(&self) -> WriteGate {
        Arc::clone(&self.write_gate.lock().expect("write gate poisoned"))
    }
    pub fn set_write_gate(&self, gate: WriteGate) {
        *self.write_gate.lock().expect("write gate poisoned") = gate;
    }
    #[must_use]
    pub fn members(&self) -> BTreeSet<String> {
        self.specs
            .keys()
            .filter(|name| self.shared_team(name).is_some())
            .cloned()
            .collect()
    }

    #[must_use]
    pub fn shared_team(&self, name: &str) -> Option<Arc<TeamRuntime>> {
        self.team
            .lock()
            .expect("crew team lock poisoned")
            .clone()
            .filter(|team| team.contains_member(name))
    }

    pub fn specs(&self) -> impl Iterator<Item = &AgentSpec> {
        self.specs.values()
    }

    #[must_use]
    pub fn is_built(&self, name: &str) -> bool {
        self.helpers
            .lock()
            .expect("crew lock poisoned")
            .get(name)
            .is_some_and(|slot| slot.built.load(Ordering::Acquire))
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

    pub fn restore_checkpoint(&self, checkpoint: UsageCheckpoint) {
        self.restore_spend(checkpoint.spend);
        if checkpoint.session == self.setup.session_id {
            *self.checkpoint_through.lock().expect("checkpoint poisoned") = checkpoint.through;
            if let Some(id) = checkpoint.through {
                self.scheduler.reserve_after(id);
            }
        }
    }
    #[must_use]
    pub fn usage_checkpoint(&self) -> UsageCheckpoint {
        UsageCheckpoint {
            session: self.setup.session_id.clone(),
            through: self
                .scheduler
                .snapshots()
                .iter()
                .map(|node| node.id)
                .max()
                .max(*self.checkpoint_through.lock().expect("checkpoint poisoned")),
            spend: self.spend_snapshot(),
        }
    }
    pub async fn recover_usage(&self) -> Result<(), String> {
        let mut recovering = self.recovering.lock().await;
        if *recovering {
            return Ok(());
        }
        self.scheduler.initialize().await?;
        let through = *self.checkpoint_through.lock().expect("checkpoint poisoned");
        for node in self
            .scheduler
            .snapshots()
            .into_iter()
            .filter(|node| through.is_none_or(|id| node.id > id))
        {
            if let Participant::Helper(name) = node.actor {
                self.spend
                    .lock()
                    .expect("crew lock poisoned")
                    .entry(name)
                    .or_default()
                    .add(&node.usage);
            }
        }
        *recovering = true;
        Ok(())
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

    /// Already cancelled calls do not consume a second Esc intended for the lead.
    pub fn stop_running(&self) -> Vec<String> {
        let scoped = self.scheduler.stop_helpers();
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
            .chain(scoped)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    pub async fn wait_for_idle(&self) {
        self.scheduler.wait_for_idle().await;
        let helpers = self
            .helpers
            .lock()
            .expect("crew lock poisoned")
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for helper in helpers {
            if let Some(helper) = helper.value.lock().await.as_mut() {
                helper.agent.wait_for_idle().await;
            }
        }
    }

    pub async fn run(
        &self,
        name: &str,
        task: &str,
        cancel: CancellationToken,
        progress: &(dyn Fn(String) + Send + Sync),
    ) -> Result<Reply, CrewError> {
        self.run_with_team(name, task, cancel, progress, self.shared_team(name))
            .await
    }

    pub async fn run_with_team(
        &self,
        name: &str,
        task: &str,
        cancel: CancellationToken,
        progress: &(dyn Fn(String) + Send + Sync),
        shared: Option<Arc<TeamRuntime>>,
    ) -> Result<Reply, CrewError> {
        let owner = self.owner.lock().expect("crew owner poisoned").upgrade();
        if let Some(owner) = owner {
            let spec = self
                .spec(name)
                .ok_or_else(|| CrewError::Unknown(name.into()))?;
            return owner
                .owned_request(
                    ScopedRequest {
                        name: name.into(),
                        task: task.into(),
                        parent: None,
                        access: if spec.definition.writes() {
                            AccessScope::CanWrite
                        } else {
                            AccessScope::ReadOnly
                        },
                        cancel,
                        shared,
                        selection: None,
                    },
                    progress,
                )
                .await;
        }
        self.run_legacy(name, task, cancel, progress, shared, None)
            .await
    }

    pub async fn run_child(
        self: &Arc<Self>,
        parent: &CallScope,
        name: &str,
        task: &str,
        access: AccessScope,
    ) -> Result<Reply, CrewError> {
        let spec = self
            .spec(name)
            .ok_or_else(|| CrewError::Unknown(name.into()))?;
        if access == AccessScope::ReadOnly && spec.definition.writes() {
            return Err(CrewError::Failed {
                name: name.into(),
                message: "a read-only subtree cannot call a writer".into(),
                changes: Changes::ReadOnly,
            });
        }
        self.owned_request(
            ScopedRequest {
                name: name.into(),
                task: task.into(),
                parent: Some(parent.clone()),
                access,
                cancel: parent.cancellation_token().child_token(),
                shared: self.shared_team(name),
                selection: None,
            },
            &|_| {},
        )
        .await
    }

    pub async fn run_selected_child(
        self: &Arc<Self>,
        parent: &CallScope,
        selection: RoleSelection,
        task: &str,
        access: AccessScope,
    ) -> Result<Reply, CrewError> {
        self.owned_request(
            ScopedRequest {
                name: selection.agent.clone(),
                task: task.into(),
                parent: Some(parent.clone()),
                access,
                cancel: parent.cancellation_token().child_token(),
                shared: self.shared_team(&selection.agent),
                selection: Some((
                    selection,
                    if access == AccessScope::ReadOnly {
                        AccessFilter::ReadOnly
                    } else {
                        AccessFilter::Writer
                    },
                )),
            },
            &|_| {},
        )
        .await
    }

    pub async fn run_selected_direct(
        self: &Arc<Self>,
        selection: RoleSelection,
        task: &str,
        cancel: CancellationToken,
        shared: Option<Arc<TeamRuntime>>,
        progress: &(dyn Fn(String) + Send + Sync),
    ) -> Result<Reply, CrewError> {
        let spec = self
            .spec(&selection.agent)
            .ok_or_else(|| CrewError::Unknown(selection.agent.clone()))?;
        if shared.is_none() {
            return Err(CrewError::Failed {
                name: selection.agent,
                message: "discovered target is no longer on the team".into(),
                changes: Changes::ReadOnly,
            });
        }
        self.owned_request(
            ScopedRequest {
                name: selection.agent.clone(),
                task: task.into(),
                parent: None,
                access: if spec.definition.writes() {
                    AccessScope::CanWrite
                } else {
                    AccessScope::ReadOnly
                },
                cancel,
                shared,
                selection: Some((selection, AccessFilter::Any)),
            },
            progress,
        )
        .await
    }

    async fn owned_request(
        self: &Arc<Self>,
        mut request: ScopedRequest,
        progress: &(dyn Fn(String) + Send + Sync),
    ) -> Result<Reply, CrewError> {
        let stop = request.cancel.child_token();
        let worker_stop = stop.clone();
        let crew = Arc::clone(self);
        let name = request.name.clone();
        let (sender, receipt) = tokio::sync::oneshot::channel();
        let (updates, mut update_events) = tokio::sync::mpsc::unbounded_channel();
        // Admission itself is owned, dropping a caller while the journal is loading
        // must not strand a queue reservation before a managed worker exists.
        tokio::spawn(async move {
            let result = async {
                let failed = |message: String| CrewError::Failed {
                    name: request.name.clone(),
                    message,
                    changes: Changes::ReadOnly,
                };
                crew.recover_usage().await.map_err(failed)?;
                if let Some((selection, filter)) = &request.selection {
                    let selection = crew
                        .validate_selection(selection, *filter, request.parent.as_ref())
                        .map_err(failed)?;
                    request.selection = Some((selection, *filter));
                }
                let scope = match &request.parent {
                    Some(parent) => {
                        crew.scheduler
                            .child(parent, &request.name, &request.task, request.access)
                            .await
                    }
                    None => {
                        crew.scheduler
                            .root(
                                Participant::Helper(request.name.clone()),
                                request.access,
                                &request.task,
                                worker_stop.clone(),
                                if request.shared.is_some() {
                                    TaskVisibility::Shared
                                } else {
                                    TaskVisibility::Private
                                },
                            )
                            .await
                    }
                }
                .map_err(failed)?;
                if worker_stop.is_cancelled() {
                    scope.cancellation_token().cancel();
                }
                if let Some((selection, _)) = &request.selection {
                    crew.scheduler
                        .annotate_selection(scope.id(), selection.clone())
                        .await;
                }
                let progress = move |text| {
                    let _ = updates.send(text);
                };
                let run = crew.managed(
                    scope.clone(),
                    request.name,
                    request.task,
                    request.shared,
                    &progress,
                    request.selection,
                );
                tokio::pin!(run);
                tokio::select! {biased;
                    ()=worker_stop.cancelled()=>{scope.cancellation_token().cancel();run.await}
                    result=&mut run=>result,
                }
            }
            .await;
            let _ = sender.send(result);
        });
        struct StopOnDrop(CancellationToken);
        impl Drop for StopOnDrop {
            fn drop(&mut self) {
                self.0.cancel();
            }
        }
        let _stop = StopOnDrop(stop);
        tokio::pin!(receipt);
        loop {
            tokio::select! {
                result=&mut receipt=>return result.map_err(|error|CrewError::Failed{name,message:format!("admission worker stopped: {error}"),changes:Changes::Unknown})?,
                Some(text)=update_events.recv()=>progress(text),
            }
        }
    }

    async fn managed(
        self: &Arc<Self>,
        scope: CallScope,
        name: String,
        task: String,
        shared: Option<Arc<TeamRuntime>>,
        progress: &(dyn Fn(String) + Send + Sync),
        selection: Option<(RoleSelection, AccessFilter)>,
    ) -> Result<Reply, CrewError> {
        let (sender, receipt) = tokio::sync::oneshot::channel();
        let (updates, mut update_events) = tokio::sync::mpsc::unbounded_channel();
        let crew = Arc::clone(self);
        let worker_scope = scope.clone();
        let worker_name = name.clone();
        // A worker owns inference, cleanup, observation and accounting even if its caller disappears.
        tokio::spawn(async move {
            let inner_crew = Arc::clone(&crew);
            let inner_scope = worker_scope.clone();
            let inner_name = worker_name.clone();
            let worker = tokio::spawn(async move {
                inner_crew
                    .scheduler
                    .acquire(&inner_scope)
                    .await
                    .map_err(|message| {
                        if inner_scope.cancellation_token().is_cancelled() {
                            CrewError::Stopped {
                                name: inner_name.clone(),
                                changes: Changes::ReadOnly,
                            }
                        } else {
                            CrewError::Failed {
                                name: inner_name.clone(),
                                message,
                                changes: Changes::ReadOnly,
                            }
                        }
                    })?;
                if let Some((selection, filter)) = selection {
                    let parent = inner_scope
                        .parent_id()
                        .and_then(|id| inner_crew.scheduler.scope(id));
                    inner_crew
                        .validate_selection(&selection, filter, parent.as_ref())
                        .map_err(|message| CrewError::Failed {
                            name: inner_name.clone(),
                            message,
                            changes: Changes::ReadOnly,
                        })?;
                }
                inner_crew
                    .run_legacy(
                        &inner_name,
                        &task,
                        inner_scope.cancellation_token(),
                        &move |text| {
                            let _ = updates.send(text);
                        },
                        shared,
                        Some(&inner_scope),
                    )
                    .await
            });
            let result = match worker.await {
                Ok(result) => result,
                Err(error) => {
                    worker_scope.cancellation_token().cancel();
                    let helper = crew
                        .helpers
                        .lock()
                        .expect("crew lock poisoned")
                        .get(&worker_name)
                        .cloned();
                    if let Some(helper) = helper
                        && let Some(helper) = helper.value.lock().await.as_mut()
                    {
                        helper.agent.wait_for_idle().await;
                    }
                    let mut usage = worker_scope.usage();
                    usage.unknown_usage = true;
                    crew.spend
                        .lock()
                        .expect("crew lock poisoned")
                        .entry(worker_name.clone())
                        .or_default()
                        .add(&usage);
                    Err(CrewError::Failed {
                        name: worker_name.clone(),
                        message: format!("helper worker failed: {error}"),
                        changes: Changes::Unknown,
                    })
                }
            };
            let (state, answer, changes) = match &result {
                Ok(reply) => (
                    if reply.stopped_after.is_some() {
                        TaskState::Cancelled
                    } else {
                        TaskState::Succeeded
                    },
                    reply.answer.clone(),
                    reply.changes.clone(),
                ),
                Err(error @ CrewError::Stopped { changes, .. }) => {
                    (TaskState::Cancelled, error.to_string(), changes.clone())
                }
                Err(error @ CrewError::Failed { changes, .. }) => (
                    if worker_scope.cancellation_token().is_cancelled() {
                        TaskState::Cancelled
                    } else {
                        TaskState::Failed(error.to_string())
                    },
                    error.to_string(),
                    changes.clone(),
                ),
                Err(error @ CrewError::Unknown(_)) => (
                    TaskState::Failed(error.to_string()),
                    error.to_string(),
                    Changes::ReadOnly,
                ),
            };
            crew.scheduler
                .finish(&worker_scope, state, &answer, changes)
                .await;
            let _ = sender.send(result);
        });
        struct StopOnDrop(CancellationToken);
        impl Drop for StopOnDrop {
            fn drop(&mut self) {
                self.0.cancel();
            }
        }
        let _stop = StopOnDrop(scope.cancellation_token());
        tokio::pin!(receipt);
        loop {
            tokio::select! {
                result=&mut receipt=>return result.map_err(|error|CrewError::Failed{name,message:format!("helper supervisor stopped: {error}"),changes:Changes::Unknown})?,
                Some(text)=update_events.recv()=>progress(text),
            }
        }
    }

    async fn run_legacy(
        &self,
        name: &str,
        task: &str,
        cancel: CancellationToken,
        progress: &(dyn Fn(String) + Send + Sync),
        shared: Option<Arc<TeamRuntime>>,
        scope: Option<&CallScope>,
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
        let _active = shared.as_ref().map(|team| team.enter(name));
        let actor = Participant::Helper(name.into());
        let mut start = None;
        if scope.is_none()
            && let Some(team) = &shared
        {
            match team.task(actor.clone(), TaskPhase::Started, None, task, "") {
                Ok(receipt) => match team.receive(receipt).await {
                    Ok(entry) => start = Some(entry.id),
                    Err(problem) => team.warn(problem),
                },
                Err(problem) => team.warn(problem),
            }
        }
        let result = self
            .run_call(spec, task, &stop, &mut on_duty, progress, scope)
            .await;
        if scope.is_none()
            && let Some(team) = shared
        {
            let (phase, text, changes) = match &result {
                Ok(reply) => (
                    if reply.stopped_after.is_some() {
                        TaskPhase::Stopped
                    } else {
                        TaskPhase::Succeeded
                    },
                    reply.stopped_after.map_or_else(
                        || reply.answer.clone(),
                        |turns| {
                            format!(
                                "[maxTurns: stopped after {turns} turns before finishing]\n{}",
                                reply.answer
                            )
                        },
                    ),
                    reply.changes.to_string(),
                ),
                Err(error @ CrewError::Stopped { changes, .. }) => {
                    (TaskPhase::Stopped, error.to_string(), changes.to_string())
                }
                Err(error @ CrewError::Failed { changes, .. }) => {
                    (TaskPhase::Failed, error.to_string(), changes.to_string())
                }
                Err(error @ CrewError::Unknown(_)) => {
                    (TaskPhase::Failed, error.to_string(), String::new())
                }
            };
            match team.task(actor, phase, start, &text, &changes) {
                Ok(receipt) => {
                    if let Err(problem) = team.receive(receipt).await {
                        team.warn(problem);
                    }
                }
                Err(problem) => team.warn(problem),
            }
        }
        result
    }

    async fn run_call(
        &self,
        spec: &AgentSpec,
        task: &str,
        stop: &CancellationToken,
        on_duty: &mut OnDuty<'_>,
        progress: &(dyn Fn(String) + Send + Sync),
        scope: Option<&CallScope>,
    ) -> Result<Reply, CrewError> {
        let name = spec.name.as_str();
        let stopped = || CrewError::Stopped {
            name: name.into(),
            changes: Changes::ReadOnly,
        };
        if stop.is_cancelled() {
            return Err(stopped());
        }
        let slot = self.helper_slot(spec);
        let mut value = tokio::select! {
            biased;
            () = stop.cancelled() => return Err(stopped()),
            value = slot.value.lock() => value,
        };
        if value.is_none() {
            let history = tokio::select! {
                biased;
                () = stop.cancelled() => return Err(stopped()),
                history = self.load_history(spec) => history,
            };
            if stop.is_cancelled() {
                return Err(stopped());
            }
            *value = Some(self.build(spec, history));
            slot.built.store(true, Ordering::Release);
        }
        let helper = value
            .as_mut()
            .expect("helper initialized under its exclusive slot lock");
        // A dropped caller cancels its inner run, never reuse its history mid run.
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
        if let Some(scope) = scope {
            helper.hooks.context.set(RunContext::ready(scope.clone()));
        }
        let mut tools = helper
            .agent
            .tools()
            .iter()
            .filter(|tool| {
                !CONVERSATION_TOOL_NAMES.contains(&tool.name())
                    && !crate::tools::team::TEAM_TOOL_NAMES.contains(&tool.name())
            })
            .cloned()
            .collect::<Vec<_>>();
        if let Some(team) = self.shared_team(name) {
            tools.extend(crate::tools::team_conversation::scoped_conversation_tools(
                &team,
                Participant::Helper(name.into()),
                Arc::clone(&self.scheduler),
                Arc::clone(&helper.hooks.context),
            ));
        }
        if let Some(scope) = scope
            && scope.visibility() == TaskVisibility::Shared
            && scope.can_delegate()
            && let Some(owner) = self.owner.lock().expect("crew owner poisoned").upgrade()
        {
            let permitted = self
                .members()
                .into_iter()
                .filter(|name| scope.can_call(name))
                .collect();
            tools.extend(crate::tools::team::scoped_team_tools(
                &owner,
                &permitted,
                Arc::clone(&helper.hooks.context),
                scope.access(),
                Some(name),
            ));
        }
        if tools.len() != helper.agent.tools().len()
            || tools.iter().zip(helper.agent.tools()).any(|(a, b)| {
                a.name() != b.name()
                    || a.description() != b.description()
                    || a.parameters() != b.parameters()
            })
        {
            helper.agent.set_tools(tools);
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
                    Some(AgentEvent::CompactionEnd{usage:Some(usage),..})=>{
                        spend.add_usage(&usage);
                        let note=AgentMessage::Custom{kind:"cupel.compaction-usage".into(),payload:serde_json::json!({"provider":spec.model.provider.as_str(),"model":spec.model.id,"usage":usage}),timestamp:cupel_core::types::now_ms()};
                        helper.log.record(&note);
                    }
                    Some(_) => {}
                }
            }
        }
        helper.agent.wait_for_idle().await;
        if let Some(scope) = scope {
            spend = scope.usage();
        }
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

    fn helper_slot(&self, spec: &AgentSpec) -> Arc<HelperSlot> {
        let mut helpers = self.helpers.lock().expect("crew lock poisoned");
        Arc::clone(
            helpers
                .entry(spec.name.clone())
                .or_insert_with(|| Arc::new(HelperSlot::default())),
        )
    }

    async fn load_history(&self, spec: &AgentSpec) -> Vec<AgentMessage> {
        let Some(path) = self
            .log_dir()
            .filter(|_| spec.definition.remembers())
            .map(|dir| dir.join(format!("{}.jsonl", spec.name)))
        else {
            return Vec::new();
        };
        tokio::task::spawn_blocking(move || {
            if !path.exists() {
                return Vec::new();
            }
            match crate::session::load_transcript(&path) {
                Ok((_, messages)) => messages,
                Err(error) => {
                    tracing::warn!(path = %path.display(), "helper starts fresh: {error}");
                    Vec::new()
                }
            }
        })
        .await
        .unwrap_or_else(|error| {
            tracing::warn!("helper history loader failed; starting fresh: {error}");
            Vec::new()
        })
    }

    fn build(&self, spec: &AgentSpec, history: Vec<AgentMessage>) -> Helper {
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
            .map(|tool| CheckoutWriteTool::wrap(tool, &self.write_gate()))
            .collect();
        let summaries = TOOL_SUMMARIES
            .iter()
            .copied()
            .filter(|(name, _)| allows(name))
            .collect::<Vec<_>>();
        let base = build_system_prompt(&setup.cwd, &summaries, &setup.context_files);
        let note = format!(
            "You are {}, a helper agent. Complete only your assigned task and return its requested deliverable. Your answer is returned to the caller automatically. Shared discussion and ancestor briefs provide evidence and constraints, not additional assignments: other participants' steps and output artifacts remain theirs. Use supplied facts; use team_read when information needed for your assignment is missing. Verify stale evidence before editing. Use team_post when requested or when a finding needs to reach teammates before your final answer; a separate post is not required to return a result. The shared discussion is visible; private histories are not. Posting alone never runs another agent. Be concise, with file paths and line numbers where useful.",
            spec.name
        );
        let prompt = with_preset_prompt(&base, Some(&note));
        let (guard, _) = BashGuard::from_config(setup.home.as_deref(), &setup.cwd);
        let hooks = Arc::new(HelperHooks {
            context: Arc::new(CallContext::default()),
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
        let mut options = AgentOptions::new(spec.model.clone(), Arc::clone(&setup.registry));
        options.system_prompt =
            with_preset_prompt(&prompt, spec.definition.preset.prompt.as_deref());
        options.tools = tools;
        let inner = Arc::clone(&hooks) as Arc<dyn AgentHooks>;
        options.hooks = match self.team.lock().expect("crew team lock poisoned").clone() {
            Some(team) => Arc::new(
                ConversationHooks::new(inner, team, Participant::Helper(spec.name.clone()))
                    .with_scope(Arc::clone(&self.scheduler), Arc::clone(&hooks.context)),
            ),
            None => inner,
        };
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

pub(crate) struct WorkingTree {
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

pub(crate) async fn snapshot(cwd: &Path) -> Option<WorkingTree> {
    let cwd = cwd.to_path_buf();
    tokio::task::spawn_blocking(move || dirty_files(&cwd))
        .await
        .ok()
        .flatten()
}

pub(crate) async fn observe_changes(before: Option<WorkingTree>, cwd: &Path) -> Changes {
    match (before, snapshot(cwd).await) {
        (Some(before), Some(after)) if before.head == after.head => {
            Changes::Files(changed(&before.files, &after.files))
        }
        _ => Changes::Unknown,
    }
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
        Nested,
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
                    Step::Nested if call == 1 => (
                        vec![AssistantContent::ToolCall(ToolCall {
                            id: "child-1".into(),
                            name: "ask".into(),
                            arguments: serde_json::json!({"agent":"leaf","task":"find the answer","context":"Keep the parser signature. Existing evidence: src/parser.rs:42."}),
                        })],
                        StopReason::ToolUse,
                    ),
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
                    Step::Say | Step::Nested => (
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

    fn scoped_crew(step: Step, limits: TeamLimits) -> (Arc<Crew>, Arc<Mock>) {
        let (mut crew, mock) = crew(step, serde_json::json!({}));
        let inner = Arc::get_mut(&mut crew).unwrap();
        inner.scheduler = TeamScheduler::new(limits);
        inner.specs.get_mut("scout").unwrap().definition.description = Some("Finds answers".into());
        let mut leaf = inner.specs["scout"].clone();
        leaf.name = "leaf".into();
        inner.specs.insert("leaf".into(), leaf);
        let (team, _) = TeamRuntime::new(None);
        team.set_members(["scout".into(), "leaf".into()].into_iter().collect());
        crew.set_team_runtime(team);
        (crew, mock)
    }

    #[tokio::test]
    async fn helpers_can_delegate_with_shared_budgets_and_own_usage() {
        let (crew, mock) = scoped_crew(Step::Nested, TeamLimits::default());
        let reply = crew
            .run(
                "scout",
                "integrate the answer",
                CancellationToken::new(),
                &|_| {},
            )
            .await
            .unwrap();
        assert_eq!(reply.answer, "answer");
        assert_eq!(mock.contexts.lock().unwrap().len(), 3);
        let child_context =
            serde_json::to_string(&mock.contexts.lock().unwrap()[1].messages).unwrap();
        assert!(child_context.contains("Keep the parser signature"));
        assert!(child_context.contains("Existing evidence: src/parser.rs:42"));
        assert!(child_context.contains("Root task context"));
        let contexts = mock.contexts.lock().unwrap();
        let child_tools = contexts[1].tools.as_ref().unwrap();
        assert!(
            !child_tools
                .iter()
                .any(|tool| matches!(tool.name.as_str(), "ask" | "delegate")),
            "this child's only peers are itself and an ancestor"
        );
        drop(contexts);
        assert_eq!(crew.spend_of("scout").input, 20);
        assert_eq!(crew.spend_of("leaf").input, 10);
        assert_eq!(crew.total_spend().input, 30);
        assert_eq!(crew.total_spend().calls, 2);
        let nodes = crew.scheduler.snapshots();
        assert_eq!(nodes.len(), 2);
        assert!(nodes.iter().all(|node| node.state.terminal()));
        let child = nodes.iter().find(|node| node.parent.is_some()).unwrap();
        assert_eq!(child.root, nodes[0].id);
        assert_eq!(child.access, AccessScope::ReadOnly);
    }

    #[tokio::test]
    async fn cancelled_unbuilt_waiter_does_not_block_another_helpers_initialization() {
        let (mut crew, mock) = crew(Step::Say, serde_json::json!({}));
        let mut other = crew.spec("scout").unwrap().clone();
        other.name = "other".into();
        Arc::get_mut(&mut crew)
            .unwrap()
            .specs
            .insert("other".into(), other);
        let slot = crew.helper_slot(crew.spec("scout").unwrap());
        let lease = slot.value.lock().await;
        let waiter = spawn(&crew);
        tokio::time::timeout(Duration::from_secs(2), async {
            while crew.running_names().is_empty() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let reply = tokio::time::timeout(
            Duration::from_secs(2),
            crew.run(
                "other",
                "independent task",
                CancellationToken::new(),
                &|_| {},
            ),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(reply.answer, "answer");
        crew.stop_running();
        assert!(matches!(
            waiter.await.unwrap(),
            Err(CrewError::Stopped { .. })
        ));
        assert!(!crew.is_built("scout"));
        assert!(crew.is_built("other"));
        assert_eq!(mock.contexts.lock().unwrap().len(), 1);
        drop(lease);
    }

    #[tokio::test]
    async fn recursive_provider_budget_stops_further_requests() {
        let (crew, mock) = scoped_crew(
            Step::Nested,
            TeamLimits {
                requests: 2,
                ..TeamLimits::default()
            },
        );
        let error = crew
            .run("scout", "integrate", CancellationToken::new(), &|_| {})
            .await
            .unwrap_err();
        assert!(error.to_string().contains("provider-request budget"));
        assert_eq!(mock.contexts.lock().unwrap().len(), 2);
        assert_eq!(crew.total_spend().input, 20);
        assert!(
            crew.scheduler
                .snapshots()
                .iter()
                .all(|node| node.state.terminal())
        );
    }

    #[tokio::test]
    async fn managed_workers_finish_accounting_after_the_caller_is_dropped() {
        let (crew, mock) = scoped_crew(Step::Hang, TeamLimits::default());
        let call = spawn(&crew);
        started(&mock, 1).await;
        call.abort();
        assert!(call.await.unwrap_err().is_cancelled());
        tokio::time::timeout(Duration::from_secs(5), crew.wait_for_idle())
            .await
            .unwrap();
        assert_eq!(crew.total_spend().input, 10);
        assert_eq!(crew.total_spend().calls, 1);
        assert!(
            crew.scheduler
                .snapshots()
                .iter()
                .all(|node| node.state.terminal())
        );
        let weak = Arc::downgrade(&crew);
        drop(crew);
        tokio::time::timeout(Duration::from_secs(5), async {
            while weak.upgrade().is_some() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("recursive tool ownership must not cycle back to the crew");
    }

    #[tokio::test]
    async fn checkpoints_recover_unfinished_usage_without_double_counting_settled_calls() {
        let (crew, _) = scoped_crew(Step::Nested, TeamLimits::default());
        crew.run("scout", "integrate", CancellationToken::new(), &|_| {})
            .await
            .unwrap();
        let checkpoint = crew.usage_checkpoint();
        let scope = crew
            .scheduler
            .root(
                Participant::Helper("scout".into()),
                AccessScope::ReadOnly,
                "unfinished",
                CancellationToken::new(),
                TaskVisibility::Shared,
            )
            .await
            .unwrap();
        crew.scheduler.acquire(&scope).await.unwrap();
        assert!(scope.request_problem().is_none());
        scope.observe_usage(Some(&Usage {
            input: 12,
            output: 3,
            ..Usage::default()
        }));
        crew.scheduler.publish(scope.id()).await;
        let resumed = Arc::new(Crew::new(
            crew.specs().cloned().collect(),
            CrewSetup {
                cwd: crew.setup.cwd.clone(),
                home: None,
                registry: Arc::clone(&crew.setup.registry),
                settings: crew.setup.settings.clone(),
                context_files: Vec::new(),
                session_id: crew.setup.session_id.clone(),
            },
        ));
        resumed.set_team_runtime(crew.shared_team("scout").unwrap());
        resumed.restore_checkpoint(checkpoint);
        resumed.recover_usage().await.unwrap();
        resumed.recover_usage().await.unwrap();
        assert_eq!(resumed.total_spend().input, 42);
        assert!(resumed.spend_of("scout").unknown_usage);
        assert!(!resumed.is_built("scout"));
        crew.scheduler
            .finish(
                &scope,
                TaskState::Cancelled,
                "interrupted",
                Changes::Unknown,
            )
            .await;
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
