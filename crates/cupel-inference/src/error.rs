use thiserror::Error;

pub type Result<T> = std::result::Result<T, InferenceError>;

#[derive(Debug, Error)]
pub enum InferenceError {
    #[error("no API provider registered for api: {0}")]
    NoApiProvider(String),

    #[error("mismatched api: {actual} expected {expected}")]
    MismatechApi { actual: String, expected: String },

    #[error("model not found: provider={provider}, model={model}")]
    ModelNotFound { provider: String, model: String },

    #[error("missing API key for provider: {provider}")]
    MissingApiKey { provider: String },

    #[error("tool not found: {0}")]
    ToolNotFound(String),

    #[error("validation failed for tool {tool}: {message}")]
    ToolValidation { tool: String, message: String },

    #[error("OAuth error: {0}")]
    OAuth(String),

    #[error("HTTP error: {0}")]
    Http(String),

    #[error("JSON error: {0}")]
    Json(String),

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
        Self::Json(error.to_string())
    }
}
