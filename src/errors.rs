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

#[cfg(test)]
mod tests {
    use super::*;
    use tonic::Status;

    #[test]
    fn test_service_error_to_status_not_found() {
        let err = ServiceError::NotFound("Bottle not found".to_string());
        let status: Status = err.into();
        assert_eq!(status.code(), tonic::Code::NotFound);
        assert_eq!(status.message(), "Bottle not found");
    }

    #[test]
    fn test_service_error_to_status_invalid_argument() {
        let err = ServiceError::InvalidArgument("Invalid ID".to_string());
        let status: Status = err.into();
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        assert_eq!(status.message(), "Invalid ID");
    }

    #[test]
    fn test_service_error_to_status_internal() {
        let err = ServiceError::Internal("Something went wrong".to_string());
        let status: Status = err.into();
        assert_eq!(status.code(), tonic::Code::Internal);
        assert_eq!(status.message(), "Something went wrong");
    }

    #[test]
    fn test_service_error_to_status_database() {
        let err = ServiceError::Database("SQL error".to_string());
        let status: Status = err.into();
        assert_eq!(status.code(), tonic::Code::Internal);
        assert!(status.message().contains("Database error"));
        assert!(status.message().contains("SQL error"));
    }

    #[test]
    fn test_rusqlite_error_to_service_error() {
        let sqlite_err = rusqlite::Error::InvalidParameterName("test".to_string());
        let service_err: ServiceError = sqlite_err.into();
        
        match service_err {
            ServiceError::Database(msg) => {
                assert!(msg.contains("InvalidParameterName") || msg.contains("test"));
            }
            _ => panic!("Expected Database error"),
        }
    }

    #[test]
    fn test_service_error_display() {
        assert_eq!(ServiceError::NotFound("foo".to_string()).to_string(), "Not Found: foo");
        assert_eq!(ServiceError::InvalidArgument("bar".to_string()).to_string(), "Invalid Argument: bar");
        assert_eq!(ServiceError::Internal("baz".to_string()).to_string(), "Internal Error: baz");
        assert_eq!(ServiceError::Database("qux".to_string()).to_string(), "Database Error: qux");
    }
}
