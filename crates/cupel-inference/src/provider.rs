use std::collections::BTreeMap;

use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{InferenceContext, InferenceStream, ModelRef, ModelSpec, ToolDefinition};

/// Options that vary per request.
///
/// The API key lives here because it is a secret and request-scoped. Do not store
/// secrets in `ModelSpec`, because model specs are configuration data that may
/// be printed, serialized, or shared.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InferenceRequestOptions {
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub max_output_tokens: Option<u32>,

    /// Provider-neutral reasoning effort level.
    ///
    /// Adapters translate this into provider-specific fields where supported.
    pub reasoning_effort: Option<ReasoningEffort>,

    /// Emit raw provider JSON events into the provider-neutral stream.
    ///
    /// This is useful while debugging adapters, but it should stay opt-in
    /// because prompts and tool results may contain private code.
    pub include_raw: bool,

    /// Provider-specific escape hatch.
    ///
    /// Just development related implementation.
    pub extra: BTreeMap<String, Value>,

    /// Secret credential injected by config or CLI code.
    ///
    /// `serde(skip)` prevents accidental serialization. The `secrecy` type
    /// prevents casual debug printing of the raw secret.
    /// Provider code must not log it.
    #[serde(skip)]
    pub api_key: Option<SecretString>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReasoningEffort {
    Low,
    Medium,
    High,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InferenceRequest {
    pub model_ref: ModelRef,
    pub context: InferenceContext,
    pub tools: Vec<ToolDefinition>,
    pub options: InferenceRequestOptions,
}

pub trait InferenceProvider: Send + Sync {
    fn stream(&self, request: ResolvedInferenceRequest) -> InferenceStream;
}

#[derive(Clone, Debug)]
pub struct ResolvedInferenceRequest {
    pub model: ModelSpec,
    pub request: InferenceRequest,
}
