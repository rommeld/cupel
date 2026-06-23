//! Provider trait + registry + the top-level `stream`/`complete` entry points.

use std::{collections::HashMap, sync::Arc};

use crate::{
    error::{InferenceError, Result},
    event_stream::AssistantMessageStream,
    types::{AssistantMessage, Context, Model, StreamOptions},
};

/// A backend that can turn (model, context, options) into a stream of events.
///
/// `stream` is intentionally synchronous: it returns the stream object
/// *immediately* and performs the HTTP work in the background. Per the
/// contract, errors after this point are delivered *on the stream*
/// (as an `Error` event), never by panicing.
pub trait Provider: Send + Sync {
    /// The API string this provider handles (the registry key).
    fn api(&self) -> &str;

    fn stream(
        &self,
        model: &Model,
        context: Context,
        options: StreamOptions,
    ) -> AssistantMessageStream;
}

/// Maps API strings to providers. No global state and tests are isolated.
/// `Arc<dyn Provider>` lets multiple call sites share one provider.
#[derive(Default, Clone)]
pub struct Registry {
    providers: HashMap<String, Arc<dyn Provider>>,
}

impl Registry {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, provider: Arc<dyn Provider>) {
        self.providers.insert(provider.api().to_string(), provider);
    }

    #[must_use]
    pub fn get(&self, api: &str) -> Option<Arc<dyn Provider>> {
        self.providers.get(api).cloned()
    }

    /// Begin streaming. The only failure mode here is "no provider registered" - everything
    /// else is reported on the returned stream.
    pub fn stream(
        &self,
        model: &Model,
        context: Context,
        options: StreamOptions,
    ) -> Result<AssistantMessageStream> {
        let provider = self
            .get(model.api.as_str())
            .ok_or_else(|| InferenceError::NoProvider(model.api.as_str().to_string()))?;
        Ok(provider.stream(model, context, options))
    }

    /// Convenience: stream to completion and return the final message.
    pub async fn complete(
        &self,
        model: &Model,
        context: Context,
        options: StreamOptions,
    ) -> Result<AssistantMessage> {
        Ok(self.stream(model, context, options)?.result().await?)
    }
}
