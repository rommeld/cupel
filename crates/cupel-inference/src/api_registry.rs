use crate::{
    error::{InferenceError, Result},
    event_stream::AssistantMessageEventStream,
    types::{Api, Context, Model, SimpleStreamOptions, StreamOptions},
};
use once_cell::sync::Lazy;
use parking_lot::RwLock;
use std::{collections::HashMap, sync::Arc};

pub type StreamFn =
    Arc<dyn Fn(Model, Context, StreamOptions) -> Result<AssistantMessageEventStream> + Send + Sync>;
pub type StreamSimpleFn = Arc<
    dyn Fn(Model, Context, SimpleStreamOptions) -> Result<AssistantMessageEventStream>
        + Send
        + Sync,
>;

#[derive(Clone)]
pub struct ApiProvider {
    pub api: Api,
    pub stream: StreamFn,
    pub stream_simple: StreamSimpleFn,
}

#[derive(Clone)]
struct RegisteredApiProvider {
    provider: ApiProvider,
    source_id: Option<String>,
}

static REGISTRY: Lazy<RwLock<HashMap<String, RegisteredApiProvider>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));

fn wrap_stream(api: Api, inner: StreamFn) -> StreamFn {
    Arc::new(move |model, context, options| {
        if model.api != api {
            return Err(InferenceError::MismatechApi {
                actual: model.api.0,
                expected: api.0.clone(),
            });
        }
        inner(model, context, options)
    })
}

fn wrap_stream_simple(api: Api, inner: StreamSimpleFn) -> StreamSimpleFn {
    Arc::new(|model, context, options| {
        if model.api != api {
            return Err(InferenceError::MismatechApi {
                actual: model.api.0,
                expected: api.0.clone(),
            });
        }
        inner(model, context, options)
    })
}

pub fn register_api_provider(provider: ApiProvider, source_id: Option<String>) {
    let api = provider.api.clone();
    let wrapped = ApiProvider {
        api: api.clone(),
        stream: wrap_stream(api.clone(), provider.stream),
        stream_simple: wrap_stream_simple(api.clone(), provider.stream_simple),
    };
    REGISTRY.write().insert(
        api.0.clone(),
        RegisteredApiProvider {
            provider: wrapped,
            source_id,
        },
    );
}

pub fn get_api_provider(api: &Api) -> Option<ApiProvider> {
    REGISTRY
        .read()
        .get(api.as_str())
        .map(|entry| entry.provider.clone())
}

pub fn get_api_providers() -> Vec<ApiProviderHandle> {
    REGISTRY
        .read()
        .values()
        .map(|entry| ApiProviderHandle {
            api: entry.provider.api.clone(),
            source_id: entry.source_id.clone(),
        })
        .collect()
}

pub fn unregister_api_providers(source_id: &str) {
    REGISTRY
        .write()
        .retain(|_, entry| entry.source_id.as_deref() != Some(source_id));
}

pub fn clear_api_providers() {
    REGISTRY.write().clear();
}
