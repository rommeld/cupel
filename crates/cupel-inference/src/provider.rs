use std::collections::BTreeMap;

use secrecy::SecretString;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{InferenceContext, InferenceStream, ModelRef, ModelSpec, ToolDefinition};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InferenceRequestOptions {
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub max_output_tokens: Option<u32>,

    /// Provider-neutral reasoning effort level.
    ///
    /// Adapters translate this into provider-specific fields where supported.
    pub reasoning_effort: Option<ReasoningEffort>,

    /// Whether raw provider events should be emitted into the stream.
    pub include_raw: bool,

    /// Provider-specific escape hatch.
    ///
    /// Just development related implementation.
    pub extra: BTreeMap<String, Value>,

    /// Optional explicit API key.
    ///
    /// CLI/config.rs code should inject this. Provider code must not log it.
    #[serde(skip)]
    pub api_key: Option<SecretString>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ReasoningEffort {
    Low,
    Medium,
    How,
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
