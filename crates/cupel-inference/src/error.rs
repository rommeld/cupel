use core::result;

use thiserror::Error;

use crate::model::ApiFamily;

pub type Result<T> = result::Result<T, InferenceError>;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum InferenceError {
    #[error("no api provider registered for api: {0}")]
    NoApiProvider(String),

    #[error("mismatched api: {actual} expected {expected}")]
    MismatchedApi { actual: String, expected: String },

    #[error("model not found: {model_ref}")]
    ModelRefNotFound { model_ref: String },

    #[error("model not found: provider={provider}, model={model}")]
    ModelNotFound { provider: String, model: String },

    #[error("missing api key for provider: {provider}")]
    MissingApiKey { provider: String },

    #[error("invalid base url: {base_url}")]
    InvalidBaseUrl { base_url: String },

    #[error("provider request failed: {message}")]
    RequestFailed { message: String },

    #[error("provider returned http status {status}: {body}")]
    ProviderHttpStatus { status: u16, body: String },

    #[error("provider protocol error: {message}")]
    ProviderProtocol { message: String },

    #[error("unsupported feature for api {api_family}: {feature}")]
    UnsupportedFeature {
        api_family: ApiFamily,
        feature: String,
    },

    #[error("tool not found: {0}")]
    ToolNotFound(String),

    #[error("validation failed for tool {tool}: {message}")]
    ToolValidation { tool: String, message: String },

    #[error("oauth error: {0}")]
    OAuth(String),

    #[error("http error: {0}")]
    Http(String),

    #[error("json error: {message}")]
    Json { message: String },

    #[error("operation cancelled")]
    Cancelled,

    #[error("{0}")]
    Message(String),
}

impl From<reqwest::Error> for InferenceError {
    fn from(error: reqwest::Error) -> Self {
        Self::Http(error.to_string())
    }
}

impl From<serde_json::Error> for InferenceError {
    fn from(error: serde_json::Error) -> Self {
        Self::Json {
            message: error.to_string(),
        }
    }
}
