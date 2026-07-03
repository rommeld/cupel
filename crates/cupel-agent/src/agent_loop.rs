//! The low-level agent loop: stream an assistant response, execute its tool
//! calls, feed the results back, repeat until the model stops asking for
//! tools (and no queued messages remain).
//!
//! Port of pi's `agent-loop.ts`. Structure map:
//!
//! | pi                        | here                          |
//! |---------------------------|-------------------------------|
//! | `agentLoop()`             | [`agent_loop`]                |
//! | `agentLoopContinue()`     | [`agent_loop_continue`]       |
//! | `runLoop()`               | `run_loop`                    |
//! | `streamAssistantResponse` | `stream_assistant_response`   |
//! | `executeToolCalls*`       | `execute_tool_calls_*`        |
//!
//! One deliberate difference: pi mutates the shared context with a *partial*
//! assistant message on every stream delta so `agent.state.streamingMessage`
//! always holds the latest snapshot. Cloning the whole message per delta is
//! wasteful in Rust; instead [`AgentEvent::MessageUpdate`] carries the raw
//! provider event and consumers accumulate exactly the state they render.

use std::sync::Arc;

use futures_util::StreamExt as _;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use cupel_core::{
    provider::Registry,
    types::{
        AssistantContent, AssistantMessage, AssistantMessageEvent, Context, Message, StopReason,
        StreamOptions, Tool, ToolCall, ToolResultMessage, now_ms,
    },
};

use crate::types::{
    AgentContext, AgentEvent, AgentHooks, AgentLoopConfig, AgentMessage, AgentTool,
    AgentToolResult, ToolExecutionMode, ToolUpdateFn,
};

// ---------------------------------------------------------------------------
// Event stream plumbing (same producer/consumer split as cupel-core)
// ---------------------------------------------------------------------------

/// Consumer handle for a run's events. `AgentEnd` is the final event.
pub struct AgentEventStream {
    rx: tokio::sync::mpsc::UnboundedReceiver<AgentEvent>,
}

impl futures_core::Stream for AgentEventStream {
    type Item = AgentEvent;
    fn poll_next(
        mut self: core::pin::Pin<&mut Self>,
        cx: &mut core::task::Context<'_>,
    ) -> core::task::Poll<Option<Self::Item>> {
        self.rx.poll_recv(cx)
    }
}

/// Producer handle used inside the loop. Cloneable so concurrent tool
/// executions can emit events too.
#[derive(Clone)]
pub struct AgentEventSink {
    tx: tokio::sync::mpsc::UnboundedSender<AgentEvent>,
}

impl AgentEventSink {
    pub fn emit(&self, event: AgentEvent) {
        // A dropped consumer is not an error; the loop simply talks into the
        // void until it finishes (matching pi, where listeners are optional).
        let _ = self.tx.send(event);
    }
}

#[must_use]
pub fn agent_event_channel() -> (AgentEventStream, AgentEventSink) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    (AgentEventStream { rx }, AgentEventSink { tx })
}

// ---------------------------------------------------------------------------
// Entry points
// ---------------------------------------------------------------------------

/// Start a run with new prompt messages: they are appended to the context,
/// announced as events, and the loop takes over.
///
/// Returns all NEW messages the run produced (prompts included).
pub async fn agent_loop(
    prompts: Vec<AgentMessage>,
    mut context: AgentContext,
    config: AgentLoopConfig,
    hooks: Arc<dyn AgentHooks>,
    registry: Arc<Registry>,
    cancel: CancellationToken,
    sink: AgentEventSink,
) -> Vec<AgentMessage> {
    let mut new_messages: Vec<AgentMessage> = prompts.clone();
    context.messages.extend(prompts.iter().cloned());

    sink.emit(AgentEvent::AgentStart);
    sink.emit(AgentEvent::TurnStart);
    for prompt in prompts {
        sink.emit(AgentEvent::MessageStart {
            message: prompt.clone(),
        });
        sink.emit(AgentEvent::MessageEnd { message: prompt });
    }

    run_loop(
        context,
        &mut new_messages,
        config,
        hooks,
        registry,
        cancel,
        &sink,
    )
    .await;
    new_messages
}

/// Errors from [`agent_loop_continue`]'s precondition checks.
#[derive(Debug, thiserror::Error)]
pub enum ContinueError {
    #[error("cannot continue: no messages in context")]
    Empty,
    #[error("cannot continue from message role: assistant")]
    EndsWithAssistant,
}

/// Continue from the existing context without adding a message (retries).
/// The last message must convert to a user or tool-result message, or the
/// provider will reject the request.
pub async fn agent_loop_continue(
    context: AgentContext,
    config: AgentLoopConfig,
    hooks: Arc<dyn AgentHooks>,
    registry: Arc<Registry>,
    cancel: CancellationToken,
    sink: AgentEventSink,
) -> Result<Vec<AgentMessage>, ContinueError> {
    match context.messages.last() {
        None => return Err(ContinueError::Empty),
        Some(AgentMessage::Llm(Message::Assistant(_))) => {
            return Err(ContinueError::EndsWithAssistant);
        }
        _ => {}
    }

    let mut new_messages: Vec<AgentMessage> = Vec::new();
    sink.emit(AgentEvent::AgentStart);
    sink.emit(AgentEvent::TurnStart);

    run_loop(
        context,
        &mut new_messages,
        config,
        hooks,
        registry,
        cancel,
        &sink,
    )
    .await;
    Ok(new_messages)
}

// ---------------------------------------------------------------------------
// Main loop
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_lines)]
async fn run_loop(
    mut context: AgentContext,
    new_messages: &mut Vec<AgentMessage>,
    mut config: AgentLoopConfig,
    hooks: Arc<dyn AgentHooks>,
    registry: Arc<Registry>,
    cancel: CancellationToken,
    sink: &AgentEventSink,
) {
    let mut first_turn = true;
    // The user may have queued steering input while the previous run wound
    // down; check before the first request.
    let mut pending_messages = hooks.steering_messages().await;

    // Outer loop: restarts when follow-up messages arrive after the agent
    // would otherwise stop.
    loop {
        let mut has_more_tool_calls = true;

        // Inner loop: one iteration = one "turn" (assistant response plus
        // its tool executions).
        while has_more_tool_calls || !pending_messages.is_empty() {
            if first_turn {
                first_turn = false;
            } else {
                sink.emit(AgentEvent::TurnStart);
            }

            // Inject queued messages before the next assistant response.
            for message in pending_messages.drain(..) {
                sink.emit(AgentEvent::MessageStart {
                    message: message.clone(),
                });
                sink.emit(AgentEvent::MessageEnd {
                    message: message.clone(),
                });
                context.messages.push(message.clone());
                new_messages.push(message);
            }

            // ---- One assistant response --------------------------------
            let message =
                stream_assistant_response(&mut context, &config, &hooks, &registry, &cancel, sink)
                    .await;
            new_messages.push(AgentMessage::Llm(Message::Assistant(message.clone())));

            if matches!(message.stop_reason, StopReason::Error | StopReason::Aborted) {
                sink.emit(AgentEvent::TurnEnd {
                    message: Box::new(AgentMessage::Llm(Message::Assistant(message))),
                    tool_results: Vec::new(),
                });
                sink.emit(AgentEvent::AgentEnd {
                    messages: new_messages.clone(),
                });
                return;
            }

            // ---- Its tool calls ------------------------------------------
            let tool_calls: Vec<ToolCall> = message
                .content
                .iter()
                .filter_map(|block| match block {
                    AssistantContent::ToolCall(tc) => Some(tc.clone()),
                    _ => None,
                })
                .collect();

            let mut tool_results: Vec<ToolResultMessage> = Vec::new();
            has_more_tool_calls = false;
            if !tool_calls.is_empty() {
                let batch = execute_tool_calls(
                    &context, &message, tool_calls, &config, &hooks, &cancel, sink,
                )
                .await;
                has_more_tool_calls = !batch.terminate;
                for result in batch.messages {
                    context
                        .messages
                        .push(AgentMessage::Llm(Message::ToolResult(result.clone())));
                    new_messages.push(AgentMessage::Llm(Message::ToolResult(result.clone())));
                    tool_results.push(result);
                }
            }

            sink.emit(AgentEvent::TurnEnd {
                message: Box::new(AgentMessage::Llm(Message::Assistant(message.clone()))),
                tool_results: tool_results.clone(),
            });

            // ---- Between-turn hooks --------------------------------------
            if let Some(update) = hooks.prepare_next_turn().await {
                if let Some(model) = update.model {
                    config.model = model;
                }
                if let Some(thinking_level) = update.thinking_level {
                    config.thinking_level = thinking_level;
                }
            }

            if hooks.should_stop_after_turn(&message, &tool_results).await {
                sink.emit(AgentEvent::AgentEnd {
                    messages: new_messages.clone(),
                });
                return;
            }

            pending_messages = hooks.steering_messages().await;
        }

        // The agent would stop here; follow-up messages restart it.
        let follow_ups = hooks.follow_up_messages().await;
        if follow_ups.is_empty() {
            break;
        }
        pending_messages = follow_ups;
    }

    sink.emit(AgentEvent::AgentEnd {
        messages: new_messages.clone(),
    });
}

// ---------------------------------------------------------------------------
// Assistant streaming
// ---------------------------------------------------------------------------

/// Stream one assistant response. This is the only place `AgentMessage`s are
/// converted to provider [`Message`]s.
///
/// Failures never propagate as `Err`: they come back as an
/// [`AssistantMessage`] with `stop_reason: Error | Aborted` (pi's `StreamFn`
/// contract).
async fn stream_assistant_response(
    context: &mut AgentContext,
    config: &AgentLoopConfig,
    hooks: &Arc<dyn AgentHooks>,
    registry: &Registry,
    cancel: &CancellationToken,
    sink: &AgentEventSink,
) -> AssistantMessage {
    // AgentMessage[] -> (transform hook) -> convert -> Message[]
    let transformed = hooks.transform_context(context.messages.clone()).await;
    let llm_messages = hooks.convert_to_llm(&transformed).await;

    let llm_context = Context {
        system_prompt: (!context.system_prompt.is_empty()).then(|| context.system_prompt.clone()),
        messages: llm_messages,
        tools: (!context.tools.is_empty()).then(|| {
            context
                .tools
                .iter()
                .map(|tool| Tool {
                    name: tool.name().to_string(),
                    description: tool.description().to_string(),
                    parameters: tool.parameters(),
                })
                .collect()
        }),
    };

    // Resolve the API key fresh for every call (expiring OAuth tokens).
    let api_key = hooks
        .api_key(config.model.provider.as_str())
        .await
        .or_else(|| config.api_key.clone());

    let options = StreamOptions {
        temperature: config.temperature,
        max_tokens: config.max_tokens,
        api_key,
        signal: Some(cancel.clone()),
        session_id: config.session_id.clone(),
        reasoning: config.thinking_level,
        ..StreamOptions::default()
    };

    let stream = match registry.stream(&config.model, llm_context, options) {
        Ok(stream) => stream,
        Err(err) => {
            // No provider registered for this API - synthesize the error
            // message the provider would have produced.
            let message = error_assistant_message(config, err.to_string());
            emit_final_message(context, sink, message.clone(), false);
            return message;
        }
    };

    let mut stream = stream;
    let mut started = false;
    let mut final_message: Option<AssistantMessage> = None;

    while let Some(event) = stream.next().await {
        match event {
            AssistantMessageEvent::Start => {
                started = true;
                // Announce the in-flight assistant message with an empty
                // shell; deltas follow as MessageUpdate events.
                sink.emit(AgentEvent::MessageStart {
                    message: AgentMessage::Llm(Message::Assistant(error_shell(config))),
                });
            }
            AssistantMessageEvent::Done { message, .. } => {
                final_message = Some(message);
                break;
            }
            AssistantMessageEvent::Error { error, .. } => {
                final_message = Some(error);
                break;
            }
            other => {
                sink.emit(AgentEvent::MessageUpdate { event: other });
            }
        }
    }

    let message = final_message.unwrap_or_else(|| {
        // Channel closed without a terminal event: a provider bug, but the
        // loop must still terminate cleanly.
        error_assistant_message(config, "stream closed before terminal event".to_string())
    });
    emit_final_message(context, sink, message.clone(), started);
    message
}

/// An empty assistant message shell used for `MessageStart` announcements.
fn error_shell(config: &AgentLoopConfig) -> AssistantMessage {
    AssistantMessage {
        content: Vec::new(),
        api: config.model.api.clone(),
        provider: config.model.provider.clone(),
        model: config.model.id.clone(),
        response_model: None,
        response_id: None,
        usage: cupel_core::types::Usage::default(),
        stop_reason: StopReason::Stop,
        error_message: None,
        timestamp: now_ms(),
    }
}

fn error_assistant_message(config: &AgentLoopConfig, error: String) -> AssistantMessage {
    AssistantMessage {
        stop_reason: StopReason::Error,
        error_message: Some(error),
        ..error_shell(config)
    }
}

fn emit_final_message(
    context: &mut AgentContext,
    sink: &AgentEventSink,
    message: AssistantMessage,
    already_started: bool,
) {
    context
        .messages
        .push(AgentMessage::Llm(Message::Assistant(message.clone())));
    if !already_started {
        sink.emit(AgentEvent::MessageStart {
            message: AgentMessage::Llm(Message::Assistant(message.clone())),
        });
    }
    sink.emit(AgentEvent::MessageEnd {
        message: AgentMessage::Llm(Message::Assistant(message)),
    });
}

// ---------------------------------------------------------------------------
// Tool execution
// ---------------------------------------------------------------------------

struct ExecutedToolCallBatch {
    messages: Vec<ToolResultMessage>,
    terminate: bool,
}

struct FinalizedToolCall {
    tool_call: ToolCall,
    result: AgentToolResult,
    is_error: bool,
}

async fn execute_tool_calls(
    context: &AgentContext,
    assistant: &AssistantMessage,
    tool_calls: Vec<ToolCall>,
    config: &AgentLoopConfig,
    hooks: &Arc<dyn AgentHooks>,
    cancel: &CancellationToken,
    sink: &AgentEventSink,
) -> ExecutedToolCallBatch {
    // One sequential-only tool in the batch forces sequential execution for
    // the whole batch (a mutating tool next to a read-only one, say).
    let has_sequential_tool = tool_calls.iter().any(|tc| {
        context
            .tools
            .iter()
            .find(|t| t.name() == tc.name)
            .is_some_and(|t| t.execution_mode() == Some(ToolExecutionMode::Sequential))
    });

    if config.tool_execution == ToolExecutionMode::Sequential || has_sequential_tool {
        execute_tool_calls_sequential(context, assistant, tool_calls, hooks, cancel, sink).await
    } else {
        execute_tool_calls_parallel(context, assistant, tool_calls, hooks, cancel, sink).await
    }
}

async fn execute_tool_calls_sequential(
    context: &AgentContext,
    assistant: &AssistantMessage,
    tool_calls: Vec<ToolCall>,
    hooks: &Arc<dyn AgentHooks>,
    cancel: &CancellationToken,
    sink: &AgentEventSink,
) -> ExecutedToolCallBatch {
    let mut finalized_calls: Vec<FinalizedToolCall> = Vec::new();
    let mut messages: Vec<ToolResultMessage> = Vec::new();

    for tool_call in tool_calls {
        sink.emit(AgentEvent::ToolExecutionStart {
            tool_call_id: tool_call.id.clone(),
            tool_name: tool_call.name.clone(),
            args: tool_call.arguments.clone(),
        });

        let finalized = match prepare_tool_call(context, assistant, &tool_call, hooks, cancel).await
        {
            Preparation::Immediate { result, is_error } => FinalizedToolCall {
                tool_call,
                result,
                is_error,
            },
            Preparation::Ready { tool, args } => {
                let executed = execute_prepared(&tool, &tool_call, args, cancel, sink).await;
                finalize_executed(assistant, tool_call, executed, hooks).await
            }
        };

        sink.emit(AgentEvent::ToolExecutionEnd {
            tool_call_id: finalized.tool_call.id.clone(),
            tool_name: finalized.tool_call.name.clone(),
            result: finalized.result.clone(),
            is_error: finalized.is_error,
        });
        let message = tool_result_message(&finalized);
        sink.emit(AgentEvent::MessageStart {
            message: AgentMessage::Llm(Message::ToolResult(message.clone())),
        });
        sink.emit(AgentEvent::MessageEnd {
            message: AgentMessage::Llm(Message::ToolResult(message.clone())),
        });
        messages.push(message);
        finalized_calls.push(finalized);

        if cancel.is_cancelled() {
            break;
        }
    }

    ExecutedToolCallBatch {
        terminate: should_terminate(&finalized_calls),
        messages,
    }
}

async fn execute_tool_calls_parallel(
    context: &AgentContext,
    assistant: &AssistantMessage,
    tool_calls: Vec<ToolCall>,
    hooks: &Arc<dyn AgentHooks>,
    cancel: &CancellationToken,
    sink: &AgentEventSink,
) -> ExecutedToolCallBatch {
    // Phase 1: prepare sequentially (start events + before-hooks run in
    // assistant source order, so permission prompts appear predictably).
    // Preparation results are either finished outcomes or ready-to-run work.
    enum Entry {
        Finalized(FinalizedToolCall),
        Ready {
            tool: Arc<dyn AgentTool>,
            tool_call: ToolCall,
            args: Value,
        },
    }
    let mut entries: Vec<Entry> = Vec::new();

    for tool_call in tool_calls {
        sink.emit(AgentEvent::ToolExecutionStart {
            tool_call_id: tool_call.id.clone(),
            tool_name: tool_call.name.clone(),
            args: tool_call.arguments.clone(),
        });

        match prepare_tool_call(context, assistant, &tool_call, hooks, cancel).await {
            Preparation::Immediate { result, is_error } => {
                // Preparation failures resolve immediately - emit their end
                // event right now, in order.
                sink.emit(AgentEvent::ToolExecutionEnd {
                    tool_call_id: tool_call.id.clone(),
                    tool_name: tool_call.name.clone(),
                    result: result.clone(),
                    is_error,
                });
                entries.push(Entry::Finalized(FinalizedToolCall {
                    tool_call,
                    result,
                    is_error,
                }));
            }
            Preparation::Ready { tool, args } => entries.push(Entry::Ready {
                tool,
                tool_call,
                args,
            }),
        }
        if cancel.is_cancelled() {
            break;
        }
    }

    // Phase 2: run the ready entries concurrently. `ToolExecutionEnd` fires
    // in COMPLETION order (that's the point of parallel mode)...
    let mut ordered: Vec<Option<FinalizedToolCall>> = Vec::new();
    let mut running = futures_util::stream::FuturesUnordered::new();

    for entry in entries {
        match entry {
            Entry::Finalized(finalized) => ordered.push(Some(finalized)),
            Entry::Ready {
                tool,
                tool_call,
                args,
            } => {
                let slot = ordered.len();
                ordered.push(None);
                let hooks = Arc::clone(hooks);
                let cancel = cancel.clone();
                let sink = sink.clone();
                let assistant = assistant.clone();
                running.push(async move {
                    let executed = execute_prepared(&tool, &tool_call, args, &cancel, &sink).await;
                    let finalized =
                        finalize_executed(&assistant, tool_call, executed, &hooks).await;
                    sink.emit(AgentEvent::ToolExecutionEnd {
                        tool_call_id: finalized.tool_call.id.clone(),
                        tool_name: finalized.tool_call.name.clone(),
                        result: finalized.result.clone(),
                        is_error: finalized.is_error,
                    });
                    (slot, finalized)
                });
            }
        }
    }
    while let Some((slot, finalized)) = running.next().await {
        ordered[slot] = Some(finalized);
    }

    // ...but tool-result MESSAGES are emitted in assistant source order, so
    // the transcript stays aligned with the tool_use blocks.
    let finalized_calls: Vec<FinalizedToolCall> = ordered.into_iter().flatten().collect();
    let mut messages: Vec<ToolResultMessage> = Vec::new();
    for finalized in &finalized_calls {
        let message = tool_result_message(finalized);
        sink.emit(AgentEvent::MessageStart {
            message: AgentMessage::Llm(Message::ToolResult(message.clone())),
        });
        sink.emit(AgentEvent::MessageEnd {
            message: AgentMessage::Llm(Message::ToolResult(message.clone())),
        });
        messages.push(message);
    }

    ExecutedToolCallBatch {
        terminate: should_terminate(&finalized_calls),
        messages,
    }
}

/// Outcome of the preflight phase for one tool call.
enum Preparation {
    /// Failed (unknown tool, blocked, aborted): resolves without executing.
    Immediate {
        result: AgentToolResult,
        is_error: bool,
    },
    /// Validated and allowed - ready to execute.
    Ready {
        tool: Arc<dyn AgentTool>,
        args: Value,
    },
}

async fn prepare_tool_call(
    context: &AgentContext,
    assistant: &AssistantMessage,
    tool_call: &ToolCall,
    hooks: &Arc<dyn AgentHooks>,
    cancel: &CancellationToken,
) -> Preparation {
    let error = |text: String| Preparation::Immediate {
        result: AgentToolResult::text(text),
        is_error: true,
    };

    let Some(tool) = context
        .tools
        .iter()
        .find(|t| t.name() == tool_call.name)
        .cloned()
    else {
        return error(format!("Tool {} not found", tool_call.name));
    };

    // Arguments must at least be a JSON object; field-level validation is
    // the tool's own serde deserialization.
    if !tool_call.arguments.is_object() {
        return error(format!(
            "Tool {} arguments must be a JSON object",
            tool_call.name
        ));
    }

    if let Some(before) = hooks.before_tool_call(assistant, tool_call).await {
        if cancel.is_cancelled() {
            return error("Operation aborted".to_string());
        }
        if before.block {
            return error(
                before
                    .reason
                    .unwrap_or_else(|| "Tool execution was blocked".to_string()),
            );
        }
    }
    if cancel.is_cancelled() {
        return error("Operation aborted".to_string());
    }

    Preparation::Ready {
        tool,
        args: tool_call.arguments.clone(),
    }
}

async fn execute_prepared(
    tool: &Arc<dyn AgentTool>,
    tool_call: &ToolCall,
    args: Value,
    cancel: &CancellationToken,
    sink: &AgentEventSink,
) -> (AgentToolResult, bool) {
    // Progress updates flow straight onto the event stream.
    let update_sink = sink.clone();
    let update_id = tool_call.id.clone();
    let update_name = tool_call.name.clone();
    let on_update: ToolUpdateFn = Arc::new(move |partial| {
        update_sink.emit(AgentEvent::ToolExecutionUpdate {
            tool_call_id: update_id.clone(),
            tool_name: update_name.clone(),
            partial,
        });
    });

    match tool
        .execute(&tool_call.id, args, cancel.clone(), Some(on_update))
        .await
    {
        Ok(result) => (result, false),
        Err(err) => (AgentToolResult::text(err.to_string()), true),
    }
}

async fn finalize_executed(
    assistant: &AssistantMessage,
    tool_call: ToolCall,
    (mut result, mut is_error): (AgentToolResult, bool),
    hooks: &Arc<dyn AgentHooks>,
) -> FinalizedToolCall {
    if let Some(after) = hooks
        .after_tool_call(assistant, &tool_call, &result, is_error)
        .await
    {
        // Field-by-field merge; omitted fields keep executed values.
        if let Some(content) = after.content {
            result.content = content;
        }
        if let Some(details) = after.details {
            result.details = Some(details);
        }
        if let Some(terminate) = after.terminate {
            result.terminate = terminate;
        }
        if let Some(override_error) = after.is_error {
            is_error = override_error;
        }
    }
    FinalizedToolCall {
        tool_call,
        result,
        is_error,
    }
}

/// Early termination requires EVERY tool in the batch to ask for it.
fn should_terminate(finalized: &[FinalizedToolCall]) -> bool {
    !finalized.is_empty() && finalized.iter().all(|f| f.result.terminate)
}

fn tool_result_message(finalized: &FinalizedToolCall) -> ToolResultMessage {
    ToolResultMessage {
        tool_call_id: finalized.tool_call.id.clone(),
        tool_name: finalized.tool_call.name.clone(),
        content: finalized.result.content.clone(),
        details: finalized.result.details.clone(),
        is_error: finalized.is_error,
        timestamp: now_ms(),
    }
}
