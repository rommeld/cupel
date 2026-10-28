//! User-defined helpers, constructed only on their first call.
//! Each helper owns its prompt, tools and private provider history. Calls to
//! the same helper serialize; cancellation also covers callers waiting in line.

use std::collections::{BTreeMap, BTreeSet, HashMap};
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
use crate::guard::BashGuard;
use crate::loop_killer::LoopKiller;
use crate::resources::ContextFile;
use crate::settings::{AgentDefinition, Settings};
use crate::system_prompt::{build_system_prompt, with_preset_prompt};

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
}

#[derive(Debug, thiserror::Error)]
pub enum CrewError {
    #[error("no agent named {0} (/team lists them)")]
    Unknown(String),
    #[error("{0} was stopped by the user")]
    Stopped(String),
    #[error("{name} failed: {message}")]
    Failed { name: String, message: String },
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
        let stopped = || CrewError::Stopped(name.into());
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
        let failed = |message: String| CrewError::Failed {
            name: name.into(),
            message,
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
                    Some(AgentEvent::MessageEnd { message: AgentMessage::Llm(Message::Assistant(assistant)) }) => {
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
        Ok(Reply {
            name: name.into(),
            answer: answer_text(&last),
            stopped_after: (last.stop_reason == StopReason::ToolUse)
                .then(|| helper.hooks.turns.load(Ordering::SeqCst)),
            tool_calls: lines.len(),
            spend,
        })
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
        let mut options = AgentOptions::new(spec.model.clone(), Arc::clone(&setup.registry));
        options.system_prompt =
            with_preset_prompt(&prompt, spec.definition.preset.prompt.as_deref());
        options.tools = tools;
        options.hooks = Arc::clone(&hooks) as Arc<dyn AgentHooks>;
        options.thinking_level = spec.definition.preset.thinking_level.level();
        options.tool_execution = ToolExecutionMode::Parallel;
        options.session_id = Some(format!("{}/{}", setup.session_id, spec.name));
        Helper {
            agent: Agent::new(options),
            hooks,
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
            self.contexts.lock().unwrap().push(context);
            let signal = options.signal.unwrap_or_default();
            self.signals.lock().unwrap().push(signal.clone());
            let (stream, sink) = assistant_message_channel();
            let step = self.step;
            let model = model.clone();
            tokio::spawn(async move {
                let _ = sink.start();
                let (content, stop) = match step {
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
                    Step::Fail => (Vec::new(), StopReason::Error),
                    Step::Hang => {
                        signal.cancelled().await;
                        (Vec::new(), StopReason::Aborted)
                    }
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
                    error_message: matches!(step, Step::Fail).then(|| "no credit".into()),
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
        let cwd =
            std::env::temp_dir().join(format!("cupel-crew-m2-{}-{}", std::process::id(), now_ms()));
        std::fs::create_dir_all(&cwd).unwrap();
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
            Err(CrewError::Stopped(_))
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
                Err(CrewError::Stopped(_))
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
        assert!(matches!(next.await.unwrap(), Err(CrewError::Stopped(_))));
    }
}
