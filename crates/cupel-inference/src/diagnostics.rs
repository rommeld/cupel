use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticErrorInfo {
    pub name: Option<String>,
    pub message: String,
    pub stack: Option<String>,
    pub code: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AssistantMessageDiagnostic {
    pub diagnostic_type: String,
    pub timestamp: i64,
    pub error: Option<DiagnosticErrorInfo>,
    pub details: Option<Map<String, Value>>,
}

pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub fn create_assistant_message_diagnostic(
    diagnostic_type: impl Into<String>,
    error: impl std::fmt::Display,
    details: Option<Map<String, Value>>,
) -> AssistantMessageDiagnostic {
    AssistantMessageDiagnostic {
        diagnostic_type: diagnostic_type.into(),
        timestamp: now_ms(),
        error: Some(DiagnosticErrorInfo {
            name: None,
            message: error.to_string(),
            stack: None,
            code: None,
        }),
        details,
    }
}
