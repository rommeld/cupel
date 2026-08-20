//! Stateful wrapper around the low-level agent loop.
//!
//! Concurrency model: the run executes on a spawned Tokio task, so
//! everything the run touches lives behind `Arc`s. State sits in
//! `Arc<Mutex<...>>` with short lock scopes; the queues likewise. [`Agent::prompt`]
//! hands back an [`AgentEventStream`] - the caller consumes events at its
//! own pace while the internal forwarder keeps [`AgentState`] up to date.

use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use futures_util::StreamExt as _;
use tokio_util::sync::CancellationToken;

use cupel_core::{
    provider::Registry,
    types::{Message, Model, ThinkingLevel},
};

use crate::agent_loop::{AgentEventSink, AgentEventStream, agent_event_channel, agent_loop};
use crate::types::{
    AgentContext, AgentEvent, AgentHooks, AgentLoopConfig, AgentMessage, AgentTool, NoHooks,
    RetryConfig, ToolExecutionMode,
};

/// Snapshot of the agent's public state. Cheap to clone except `messages`.
#[derive(Clone)]
pub struct AgentState {
    pub system_prompt: String,
    pub model: Model,
    /// `None` = thinking off.
    pub thinking_level: Option<ThinkingLevel>,
    pub messages: Vec<AgentMessage>,
    /// True while a run is active.
    pub is_streaming: bool,
    /// Tool call ids currently executing.
    pub pending_tool_calls: HashSet<String>,
    /// Error from the most recent failed/aborted assistant turn.
    pub error_message: Option<String>,
}

pub struct AgentOptions {
    pub system_prompt: String,
    pub model: Model,
    pub thinking_level: Option<ThinkingLevel>,
    pub tools: Vec<Arc<dyn AgentTool>>,
    pub hooks: Arc<dyn AgentHooks>,
    pub registry: Arc<Registry>,
    pub api_key: Option<String>,
    pub session_id: Option<String>,
    pub temperature: Option<f64>,
    pub max_tokens: Option<u64>,
    pub tool_execution: ToolExecutionMode,
    pub retry: RetryConfig,
    pub compaction: crate::compaction::CompactionConfig,
    pub messages: Vec<AgentMessage>,
}

impl AgentOptions {
    #[must_use]
    pub fn new(model: Model, registry: Arc<Registry>) -> Self {
        Self {
            system_prompt: String::new(),
            model,
            thinking_level: None,
            tools: Vec::new(),
            hooks: Arc::new(NoHooks),
            registry,
            api_key: None,
            session_id: None,
            temperature: None,
            max_tokens: None,
            tool_execution: ToolExecutionMode::default(),
            retry: RetryConfig::default(),
            compaction: crate::compaction::CompactionConfig::default(),
            messages: Vec::new(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("agent is already processing a prompt; wait for completion")]
    Busy,
}

pub struct Agent {
    state: Arc<Mutex<AgentState>>,
    tools: Vec<Arc<dyn AgentTool>>,
    hooks: Arc<dyn AgentHooks>,
    registry: Arc<Registry>,
    api_key: Option<String>,
    session_id: Option<String>,
    temperature: Option<f64>,
    max_tokens: Option<u64>,
    tool_execution: ToolExecutionMode,
    retry: RetryConfig,
    compaction: crate::compaction::CompactionConfig,
    /// The active run: cancel token + its join handle.
    active: Option<(CancellationToken, tokio::task::JoinHandle<()>)>,
}

impl Agent {
    #[must_use]
    pub fn new(options: AgentOptions) -> Self {
        Self {
            state: Arc::new(Mutex::new(AgentState {
                system_prompt: options.system_prompt,
                model: options.model,
                thinking_level: options.thinking_level,
                messages: options.messages,
                is_streaming: false,
                pending_tool_calls: HashSet::new(),
                error_message: None,
            })),
            tools: options.tools,
            hooks: options.hooks,
            registry: options.registry,
            api_key: options.api_key,
            session_id: options.session_id,
            temperature: options.temperature,
            max_tokens: options.max_tokens,
            tool_execution: options.tool_execution,
            retry: options.retry,
            compaction: options.compaction,
            active: None,
        }
    }

    /// Snapshot of the current state.
    #[must_use]
    pub fn state(&self) -> AgentState {
        self.state
            .lock()
            .expect("agent state lock poisoned")
            .clone()
    }

    /// Switch the model for FUTURE requests (an in-flight run keeps the
    /// model it started with; the next run picks this up).
    pub fn set_model(&self, model: Model) {
        self.state.lock().expect("agent state lock poisoned").model = model;
    }

    /// Swap the fallback API key used by FUTURE runs (the TUI's /provider
    /// and cross-provider /model switches). A run already in flight keeps
    /// the key it was started with; hook-provided keys still win.
    pub fn set_api_key(&mut self, api_key: Option<String>) {
        self.api_key = api_key;
    }

    /// The fallback API key FUTURE runs will use - the read half of
    /// [`AGENT::set_api_key`], so frontends and tests can verify which
    /// credential a reload or switch resolved without sending a request.
    #[must_use]
    pub fn api_key(&self) -> Option<&str> {
        self.api_key.as_deref()
    }

    /// The provider registry this agent dispatches through. Cheap (Arc
    /// clone); lets a frontend REBUILD an agent - the TUI's /hot-reload -
    /// without re-plumbing the registry from startup.
    #[must_use]
    pub fn registry(&self) -> Arc<Registry> {
        Arc::clone(&self.registry)
    }

    /// Set the thinking level for future requests (`None` = off).
    pub fn set_thinking_level(&self, level: Option<ThinkingLevel>) {
        self.state
            .lock()
            .expect("agent state lock poisoned")
            .thinking_level = level;
    }

    /// The thinking level FUTURE runs will use - the read half of
    /// [`Agent::set_thinking_level`], for status displays. A cheap
    /// copy read under the lock, deliberately NOT a full state()
    /// snapshot (which clones the message history) - this runs per
    /// rendered frame.
    #[must_use]
    pub fn thinking_level(&self) -> Option<ThinkingLevel> {
        self.state
            .lock()
            .expect("agent state lock poisoned")
            .thinking_level
    }

    /// Whether the CURRENT model supports reasoning at all - drives
    /// whether a thinking level is worth displaying.
    #[must_use]
    pub fn model_supports_reasoning(&self) -> bool {
        self.state
            .lock()
            .expect("agent state lock poisoned")
            .model
            .reasoning
    }

    pub fn reset(&self) {
        {
            let mut state = self.state.lock().expect("agent state lock poisoned");
            state.messages.clear();
            state.error_message = None;
        }
    }

    /// Cancellation token of the active run, if any (e.g. for a Ctrl-C
    /// handler).
    #[must_use]
    pub fn cancel_token(&self) -> Option<CancellationToken> {
        self.active.as_ref().map(|(token, _)| token.clone())
    }

    /// Abort the current run, if one is active.
    pub fn abort(&self) {
        if let Some((token, _)) = &self.active {
            token.cancel();
        }
    }

    /// Wait until the active run (if any) has fully finished.
    pub async fn wait_for_idle(&mut self) {
        if let Some((_, handle)) = self.active.take() {
            let _ = handle.await;
        }
    }

    /// Start a run with a plain text prompt.
    ///
    /// Returns the run's event stream. Consume it (or drop it - state still
    /// updates) and call [`Agent::wait_for_idle`] before the next prompt.
    pub fn prompt_text(&mut self, text: impl Into<String>) -> Result<AgentEventStream, AgentError> {
        self.prompt(vec![AgentMessage::user_text(text)])
    }

    /// Start a run with prepared prompt messages.
    pub fn prompt(&mut self, prompts: Vec<AgentMessage>) -> Result<AgentEventStream, AgentError> {
        if self
            .active
            .as_ref()
            .is_some_and(|(_, handle)| !handle.is_finished())
        {
            return Err(AgentError::Busy);
        }

        let (public_stream, public_sink) = agent_event_channel();
        let (internal_stream, internal_sink) = agent_event_channel();
        let cancel = CancellationToken::new();

        // Snapshot everything the run needs.
        let (context, config) = {
            let state = self.state.lock().expect("agent state lock poisoned");
            (
                AgentContext {
                    system_prompt: state.system_prompt.clone(),
                    messages: state.messages.clone(),
                    tools: self.tools.clone(),
                },
                AgentLoopConfig {
                    model: state.model.clone(),
                    thinking_level: state.thinking_level,
                    api_key: self.api_key.clone(),
                    session_id: self.session_id.clone(),
                    temperature: self.temperature,
                    max_tokens: self.max_tokens,
                    tool_execution: self.tool_execution,
                    retry: self.retry,
                    compaction: self.compaction,
                },
            )
        };

        // The run's hooks = user hooks + our queue draining.
        let hooks: Arc<dyn AgentHooks> = Arc::new(RunHooks {
            inner: Arc::clone(&self.hooks),
        });
        let registry = Arc::clone(&self.registry);

        {
            let mut state = self.state.lock().expect("agent state lock poisoned");
            state.is_streaming = true;
            state.error_message = None;
        }

        // Task 1: the loop itself, emitting into the internal channel.
        let loop_cancel = cancel.clone();
        tokio::spawn(async move {
            let _ = agent_loop(
                prompts,
                context,
                config,
                hooks,
                registry,
                loop_cancel,
                internal_sink,
            )
            .await;
        });

        // Task 2: forwarder - reduces every event into AgentState (pi's
        // `processEvents`), then re-emits it to the caller.
        let state = Arc::clone(&self.state);
        let handle = tokio::spawn(async move {
            forward_events(internal_stream, &state, &public_sink).await;
            let mut state = state.lock().expect("agent state lock poisoned");
            state.is_streaming = false;
            state.pending_tool_calls.clear();
        });

        self.active = Some((cancel, handle));
        Ok(public_stream)
    }
}

/// Reduce loop events into shared state, then forward them.
async fn forward_events(
    mut events: AgentEventStream,
    state: &Arc<Mutex<AgentState>>,
    sink: &AgentEventSink,
) {
    while let Some(event) = events.next().await {
        {
            let mut state = state.lock().expect("agent state lock poisoned");
            match &event {
                AgentEvent::MessageEnd { message } => {
                    state.messages.push(message.clone());
                }
                AgentEvent::ToolExecutionEnd { tool_call_id, .. } => {
                    state.pending_tool_calls.remove(tool_call_id);
                }
                AgentEvent::TurnEnd { message, .. } => {
                    if let AgentMessage::Llm(Message::Assistant(assistant)) = message.as_ref()
                        && let Some(error) = &assistant.error_message
                    {
                        state.error_message = Some(error.clone());
                    }
                }
                _ => {}
            }
        }
        sink.emit(event);
    }
}

/// Hook decorator that adds the Agent's queue draining on top of user hooks.
/// (pi builds the same thing inline in `createLoopConfig`.)
struct RunHooks {
    inner: Arc<dyn AgentHooks>,
}

#[async_trait::async_trait]
impl AgentHooks for RunHooks {
    async fn convert_to_llm(&self, messages: &[AgentMessage]) -> Vec<Message> {
        self.inner.convert_to_llm(messages).await
    }
    async fn transform_context(&self, messages: Vec<AgentMessage>) -> Vec<AgentMessage> {
        self.inner.transform_context(messages).await
    }
    async fn api_key(&self, provider: &str) -> Option<String> {
        self.inner.api_key(provider).await
    }
    async fn before_tool_call(
        &self,
        assistant: &cupel_core::types::AssistantMessage,
        tool_call: &cupel_core::types::ToolCall,
    ) -> Option<crate::types::BeforeToolCallResult> {
        self.inner.before_tool_call(assistant, tool_call).await
    }
    async fn should_stop_after_turn(
        &self,
        message: &cupel_core::types::AssistantMessage,
        tool_results: &[cupel_core::types::ToolResultMessage],
    ) -> bool {
        self.inner
            .should_stop_after_turn(message, tool_results)
            .await
    }
}
