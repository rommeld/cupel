use core::{
    mem,
    pin::Pin,
    task::{Context as TaskContext, Poll},
};
use std::{collections::BTreeMap, sync::Arc};

use futures::{
    FutureExt, Stream,
    future::{BoxFuture, Shared},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot};
use tokio_stream::wrappers::ReceiverStream;

use crate::{context::AssistantMessage, error::InferenceError};

pub type InferenceStream = Pin<Box<dyn Stream<Item = AssistantMessageEvent> + Send + 'static>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum FinishReason {
    Stop,
    Length,
    ToolCalls,
    ContentFilter,
    Error,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: Option<String>,
    pub index: usize,
    pub name: String,
    pub arguments: Value,
    pub raw_arguments: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallDelta {
    pub id: Option<String>,
    pub index: usize,
    pub name: Option<String>,
    pub arguments_delta: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum AssistantMessageDelta {
    Text { delta: String },
    Thinking { delta: String },
    ToolCall { delta: ToolCallDelta },
}

#[derive(Debug)]
pub enum AssistantMessageEvent {
    Start {
        message: AssistantMessage,
    },
    TextDelta {
        delta: String,
        message: AssistantMessage,
    },
    ThinkingDelta {
        delta: String,
        message: AssistantMessage,
    },
    ToolCallDelta {
        delta: ToolCallDelta,
        message: AssistantMessage,
    },
    Done {
        message: AssistantMessage,
    },
    Error {
        error: InferenceError,
        message: AssistantMessage,
    },
    RawProviderEvent {
        provider: String,
        payload: Value,
    },
}

impl AssistantMessageEvent {
    #[must_use]
    pub fn final_message(&self) -> Option<AssistantMessage> {
        match self {
            Self::Done { message } | Self::Error { message, .. } => Some(message.clone()),
            Self::Start { .. }
            | Self::TextDelta { .. }
            | Self::ThinkingDelta { .. }
            | Self::ToolCallDelta { .. }
            | Self::RawProviderEvent { .. } => None,
        }
    }
}

#[derive(Debug, Default)]
pub struct ToolCallAccumulator {
    calls: BTreeMap<usize, PartialToolCall>,
}

#[derive(Debug, Default)]
struct PartialToolCall {
    id: Option<String>,
    index: usize,
    name: Option<String>,
    raw_arguments: String,
}

impl ToolCallAccumulator {
    pub fn push_delta(&mut self, delta: ToolCallDelta) {
        let call = self
            .calls
            .entry(delta.index)
            .or_insert_with(|| PartialToolCall {
                index: delta.index,
                ..PartialToolCall::default()
            });

        if let Some(id) = delta.id {
            call.id = Some(id);
        }

        if let Some(name) = delta.name {
            call.name = Some(name);
        }

        if let Some(arguments_delta) = delta.arguments_delta {
            call.raw_arguments.push_str(&arguments_delta);
        }
    }

    pub fn replace_arguments(&mut self, index: usize, raw_arguments: String) -> Option<String> {
        let call = self.calls.entry(index).or_insert_with(|| PartialToolCall {
            index,
            ..PartialToolCall::default()
        });

        let suffix = raw_arguments
            .strip_prefix(&call.raw_arguments)
            .map_or_else(|| raw_arguments.clone(), str::to_owned);

        call.raw_arguments = raw_arguments;
        Some(suffix)
    }

    pub fn finish_all(&mut self) -> Vec<Result<ToolCall, serde_json::Error>> {
        mem::take(&mut self.calls)
            .into_values()
            .map(PartialToolCall::finish)
            .collect()
    }
}

impl PartialToolCall {
    fn finish(self) -> Result<ToolCall, serde_json::Error> {
        let arguments = if self.raw_arguments.trim().is_empty() {
            Value::Object(serde_json::Map::new())
        } else {
            serde_json::from_str(&self.raw_arguments)?
        };

        Ok(ToolCall {
            id: self.id,
            index: self.index,
            name: self.name.unwrap_or_default(),
            arguments,
            raw_arguments: self.raw_arguments,
        })
    }
}

pub struct AssistantMessageEventStream {
    receiver: ReceiverStream<AssistantMessageEvent>,
    result: Shared<BoxFuture<'static, AssistantMessage>>,
}

#[derive(Clone)]
pub struct AssistantMessageEventStreamWriter {
    sender: mpsc::Sender<AssistantMessageEvent>,
    final_sender: Arc<parking_lot::Mutex<Option<oneshot::Sender<AssistantMessage>>>>,
}

/// Creates a bounded assistant-message event stream and writer pair.
///
/// # Panics
///
/// The returned stream result future panics if all writers are dropped before a
/// final assistant message is sent.
#[must_use]
pub fn assistant_message_event_stream(
    buffer: usize,
) -> (
    AssistantMessageEventStreamWriter,
    AssistantMessageEventStream,
) {
    let (event_tx, event_rx) = mpsc::channel(buffer);
    let (final_tx, final_rx) = oneshot::channel::<AssistantMessage>();
    let result = async move {
        final_rx
            .await
            .expect("assistant message stream ended without a final message")
    }
    .boxed()
    .shared();

    (
        AssistantMessageEventStreamWriter {
            sender: event_tx,
            final_sender: Arc::new(parking_lot::Mutex::new(Some(final_tx))),
        },
        AssistantMessageEventStream {
            receiver: ReceiverStream::new(event_rx),
            result,
        },
    )
}

impl AssistantMessageEventStreamWriter {
    pub async fn push(&self, event: AssistantMessageEvent) {
        if let Some(final_message) = event.final_message()
            && let Some(sender) = self.final_sender.lock().take()
            && sender.send(final_message).is_err()
        {
            // The result receiver was dropped; the event channel remains authoritative.
        }
        if self.sender.send(event).await.is_err() {
            // The stream receiver was dropped; there is no consumer left to notify.
        }
    }

    pub fn try_push(&self, event: AssistantMessageEvent) {
        if let Some(final_message) = event.final_message()
            && let Some(sender) = self.final_sender.lock().take()
            && sender.send(final_message).is_err()
        {
            // The result receiver was dropped; the event channel remains authoritative.
        }
        if self.sender.try_send(event).is_err() {
            // The stream receiver was dropped or the channel is full; try_push is best-effort.
        }
    }
}

impl AssistantMessageEventStream {
    pub async fn result(&self) -> AssistantMessage {
        self.result.clone().await
    }
}

impl Stream for AssistantMessageEventStream {
    type Item = AssistantMessageEvent;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.receiver).poll_next(cx)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.receiver.size_hint()
    }
}
