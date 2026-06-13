
use thiserror::Error;

pub type Result<T> = std::result::Result<T, InferenceError>;

#[derive(Debug, Error)]
pub enum InferenceError {
    #[error("no API provider registered for api: {0}")]
    NoApiProvider(String),
    
    #[error("model not found: provider={provider}, model={model}")]
    ModelNotFound { provider: String, model: String},

    #[error("no provider registered for API family: {0}")]
    ProviderNotRegistered(ApiFamily),

    #[error("missing API key for provider: {provider}")]
    MissingApiKey { provider: String },

    #[error("invalid provider base URL: {base_url}")]
    InvalidBaseUrl { base_url: String },

    #[error("provider returned HTTP {status}: {body}")]
    ProviderHttpStatus { status: u16, body: String },

    #[error("provider protocol error: {message}")]
    ProviderProtocol { message: String },

    #[error("request failed: {message}")]
    RequestFailed { message: String },

    #[error("JSON serialization error: {message}")]
    Json { message: String },

    #[error("unsupported feature for API family {api_family}: {feature}")]
    UnsupportedFeature {
        api_family: ApiFamily,
        feature: String,
    },
}
