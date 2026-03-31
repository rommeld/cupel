use thiserror::Error;
use tonic::Status;

#[derive(Debug, Error)]
pub enum ServiceError {
    #[error("Not Found: {0}")]
    NotFound(String),

    #[error("Invalid Argument: {0}")]
    InvalidArgument(String),

    #[error("Internal Error: {0}")]
    Internal(String),

    #[error("Database Error: {0}")]
    Database(String),
}

impl From<ServiceError> for Status {
    fn from(err: ServiceError) -> Self {
        match err {
            ServiceError::NotFound(msg) => Status::not_found(msg),
            ServiceError::InvalidArgument(msg) => Status::invalid_argument(msg),
            ServiceError::Internal(msg) => Status::internal(msg),
            ServiceError::Database(msg) => Status::internal(format!("Database error: {}", msg)),
        }
    }
}

impl From<rusqlite::Error> for ServiceError {
    fn from(err: rusqlite::Error) -> Self {
        ServiceError::Database(err.to_string())
    }
}
