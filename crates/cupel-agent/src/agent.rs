//! Stateful wrapper around the low-level agent loop.
//!
//! Port of pi's `agent.ts` `Agent` class. It owns the transcript, tracks
//! streaming state, and exposes the queueing API (steer / follow-up).
//!
//! Concurrency model, since this is the one place the Rust design must
//! diverge from a JS class: the run executes on a spawned Tokio task, so
//! everything the run touches lives behind `Arc`s. State sits in
//! `Arc<Mutex<...>>` with short lock scopes; the queues likewise. Instead of
//! pi's awaited listener callbacks, [`Agent::prompt`] hands back an
//! [`AgentEventStream`] - the caller consumes events at its own pace while
//! the internal forwarder keeps [`AgentState`] up to date.

use std::collections::{HashSet, VecDeque};
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
    QueueMode, RetryConfig, ToolExecutionMode,
};

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Queues
// ---------------------------------------------------------------------------

/// A queue of messages waiting for their drain point (pi's
/// `PendingMessageQueue`).
struct PendingQueue {
    messages: VecDeque<AgentMessage>,
    mode: QueueMode,
}

impl PendingQueue {
    fn new(mode: QueueMode) -> Self {
        Self {
            messages: VecDeque::new(),
            mode,
        }
    }

    fn drain(&mut self) -> Vec<AgentMessage> {
        match self.mode {
            QueueMode::All => self.messages.drain(..).collect(),
            QueueMode::OneAtATime => self.messages.pop_front().into_iter().collect(),
        }
    }
}

// ---------------------------------------------------------------------------
// Agent
// ---------------------------------------------------------------------------

/// Options for constructing an [`Agent`].
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
    pub steering_mode: QueueMode,
    pub follow_up_mode: QueueMode,
}

impl AgentOptions {
    /// Minimal options: everything else defaulted.
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
            steering_mode: QueueMode::default(),
            follow_up_mode: QueueMode::default(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error(
        "agent is already processing a prompt; use steer()/follow_up() to queue messages, or wait for completion"
    )]
    Busy,
}

pub struct Agent {
    state: Arc<Mutex<AgentState>>,
    tools: Vec<Arc<dyn AgentTool>>,
    steering: Arc<Mutex<PendingQueue>>,
    follow_up: Arc<Mutex<PendingQueue>>,
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
                messages: Vec::new(),
                is_streaming: false,
                pending_tool_calls: HashSet::new(),
                error_message: None,
            })),
            tools: options.tools,
            steering: Arc::new(Mutex::new(PendingQueue::new(options.steering_mode))),
            follow_up: Arc::new(Mutex::new(PendingQueue::new(options.follow_up_mode))),
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

    /// Queue a message to be injected after the current assistant turn.
    pub fn steer(&self, message: AgentMessage) {
        self.steering
            .lock()
            .expect("steering lock poisoned")
            .messages
            .push_back(message);
    }

    /// Queue a message to run only after the agent would otherwise stop.
    pub fn follow_up(&self, message: AgentMessage) {
        self.follow_up
            .lock()
            .expect("follow-up lock poisoned")
            .messages
            .push_back(message);
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
            steering: Arc::clone(&self.steering),
            follow_up: Arc::clone(&self.follow_up),
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
                AgentEvent::ToolExecutionStart { tool_call_id, .. } => {
                    state.pending_tool_calls.insert(tool_call_id.clone());
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
    steering: Arc<Mutex<PendingQueue>>,
    follow_up: Arc<Mutex<PendingQueue>>,
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
    async fn after_tool_call(
        &self,
        assistant: &cupel_core::types::AssistantMessage,
        tool_call: &cupel_core::types::ToolCall,
        result: &crate::types::AgentToolResult,
        is_error: bool,
    ) -> Option<crate::types::AfterToolCallResult> {
        self.inner
            .after_tool_call(assistant, tool_call, result, is_error)
            .await
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
    async fn prepare_next_turn(&self) -> Option<crate::types::AgentLoopTurnUpdate> {
        self.inner.prepare_next_turn().await
    }

    // The queue methods are OURS; user hooks' steering/follow-up (if any)
    // are drained first, then the Agent queues.
    async fn steering_messages(&self) -> Vec<AgentMessage> {
        let mut messages = self.inner.steering_messages().await;
        messages.extend(
            self.steering
                .lock()
                .expect("steering lock poisoned")
                .drain(),
        );
        messages
    }
    async fn follow_up_messages(&self) -> Vec<AgentMessage> {
        let mut messages = self.inner.follow_up_messages().await;
        messages.extend(
            self.follow_up
                .lock()
                .expect("follow-up lock poisoned")
                .drain(),
        );
        messages
    }
}
