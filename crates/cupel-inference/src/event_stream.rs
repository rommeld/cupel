use std::{
    pin::Pin,
    sync::Arc,
    task::{Context as TaskContext, Poll},
};

use futures::{Stream, future::BoxFuture};
use tokio::sync::{mpsc, oneshot};
use tokio_stream::wrappers::ReceiverStream;

use crate::types::{AssistantMessage, AssistantMessageEvent};

pub struct AssistantMessageEventStream {
    receiver: ReceiverStream<AssistantMessageEvent>,
    result: futures::future::Shared<BoxFuture<'static, AssistantMessage>>,
}

#[derive(Clone)]
pub struct AssistantMessageEventStreamWriter {
    sender: mpsc::Sender<AssistantMessageEvent>,
    final_sender: Arc<parking_lot::Mutex<Option<oneshot::Sender<AssistantMessage>>>>,
}

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
        if let Some(final_message) = event.final_message() {
            if let Some(sender) = self.final_sender.lock().take() {
                let _ = sender.send(final_message);
            }
        }
        let _ = self.sender.send(event).await;
    }

    pub fn try_push(&self, event: AssistantMessageEvent) {
        if let Some(final_message) = event.final_message() {
            if let Some(sender) = self.final_sender.lock().take() {
                let _ = sender.send(final_message);
            }
        }
        let _ = self.sender.try_send(event);
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
}
