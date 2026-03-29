use thiserror::Error;

#[derive(Debug, Error)]
pub enum ServiceError {
    #[error("Not Found: {0}")]
    NotFound(String),
}
