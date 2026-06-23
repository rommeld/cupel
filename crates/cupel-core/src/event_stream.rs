//! The async stream primitive.
//!
//! Unbounded mspc channel gives the producer/consumer split, correct wakeups,
//! and `Stream` integration.
//!
//! Producer: provider holds `EventSink` and emits events.
//! Consumer: `AssistantMessageStream` is a `future::Stream`, and `result()`
//! consumes it to yield the final `AssistantMessage`.

use core::{
    pin::Pin,
    task::{Context as TaskContext, Poll},
};

use futures_core::Stream;
use tokio::sync::mpsc;

use crate::error::MessageStreamError;
use crate::types::{AssistantMessage, AssistantMessageEvent, StopReason, ToolCall};

/// Consumer handle: iterate events, or call `result()` for jus the final message.
pub struct AssistantMessageStream {
    rx: mpsc::UnboundedReceiver<AssistantMessageEvent>,
}

/// Producer handle held by a provider's background task.
pub struct EventSink {
    tx: mpsc::UnboundedSender<AssistantMessageEvent>,
}

/// Create linked pair (stream, sink). The provider keeps the sink and hands
/// the stream back to the caller - mirroring how `streamAnthropic()` returns a
/// stream object immediately while continues in the background.
#[must_use]
pub fn assistant_message_channel() -> (AssistantMessageStream, EventSink) {
    let (tx, rx) = mpsc::unbounded_channel();
    (AssistantMessageStream { rx }, EventSink { tx })
}

impl EventSink {
    /// Emit one event. Returns `false` if the consumer has dropped the stream -
    /// the provider task should treat that so "stop working".
    #[must_use]
    pub fn emit(&self, event: AssistantMessageEvent) -> bool {
        self.tx.send(event).is_ok()
    }

    #[must_use]
    pub fn start(&self) -> bool {
        self.emit(AssistantMessageEvent::Start)
    }
    #[must_use]
    pub fn text_start(&self, i: usize) -> bool {
        self.emit(AssistantMessageEvent::TextStart { content_index: i })
    }
    #[must_use]
    pub fn text_delta(&self, i: usize, delta: String) -> bool {
        self.emit(AssistantMessageEvent::TextDelta {
            content_index: i,
            delta,
        })
    }
    #[must_use]
    pub fn text_end(&self, i: usize, content: String) -> bool {
        self.emit(AssistantMessageEvent::TextEnd {
            content_index: i,
            content,
        })
    }
    #[must_use]
    pub fn thinking_start(&self, i: usize) -> bool {
        self.emit(AssistantMessageEvent::ThinkingStart { content_index: i })
    }
    #[must_use]
    pub fn thinking_delta(&self, i: usize, delta: String) -> bool {
        self.emit(AssistantMessageEvent::ThinkingDelta {
            content_index: i,
            delta,
        })
    }
    #[must_use]
    pub fn thinking_end(&self, i: usize, content: String) -> bool {
        self.emit(AssistantMessageEvent::ThinkingEnd {
            content_index: i,
            content,
        })
    }
    #[must_use]
    pub fn toolcall_start(&self, i: usize) -> bool {
        self.emit(AssistantMessageEvent::ToolCallStart { content_index: i })
    }
    #[must_use]
    pub fn toolcall_delta(&self, i: usize, delta: String) -> bool {
        self.emit(AssistantMessageEvent::ToolCallDelta {
            content_index: i,
            delta,
        })
    }
    #[must_use]
    pub fn toolcall_end(&self, i: usize, tool_call: ToolCall) -> bool {
        self.emit(AssistantMessageEvent::ToolCallEnd {
            content_index: i,
            tool_call,
        })
    }
    #[must_use]
    pub fn done(&self, reason: StopReason, message: AssistantMessage) -> bool {
        self.emit(AssistantMessageEvent::Done { reason, message })
    }
    #[must_use]
    pub fn error(&self, reason: StopReason, error: AssistantMessage) -> bool {
        self.emit(AssistantMessageEvent::Error { reason, error })
    }
}

// Implementing `Stream` is what lets callers write `while let Some(ev) =
// stream.next().await`. `UnboundedReceiver` is `Unpin`.
impl Stream for AssistantMessageStream {
    type Item = AssistantMessageEvent;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
        self.rx.poll_recv(cx)
    }
}

impl AssistantMessageStream {
    /// Drive the stream to completion and return the final message. It consumes the
    /// stream, called for the result not the delta.
    ///
    /// Returns `Ok` for a `Done` event, `Err(ProviderError)` for an `Error`
    /// event, and `Err(ClosedBeforeTerminalEvent)` if the channel closes
    /// without a terminal event.
    pub async fn result(mut self) -> Result<AssistantMessage, MessageStreamError> {
        while let Some(event) = self.rx.recv().await {
            match event {
                AssistantMessageEvent::Done { message, .. } => return Ok(message),
                AssistantMessageEvent::Error { reason, error } => {
                    return Err(MessageStreamError::ProviderError {
                        reason,
                        message: Box::new(error),
                    });
                }
                _ => {}
            }
        }
        Err(MessageStreamError::ClosedBeforeTerminalEvent)
    }
}
