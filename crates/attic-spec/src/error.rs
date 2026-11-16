use serde::Serialize;

pub type Result<T> = std::result::Result<T, SpecError>;

#[derive(Debug, thiserror::Error, Serialize)]
#[error("{code}: {message}")]
pub struct SpecError {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<u64>,
}

impl SpecError {
    pub fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            retry_after_ms: None,
        }
    }

    pub fn busy() -> Self {
        Self {
            code: "ERR_BUSY".into(),
            message: "workflow resource is busy; retry".into(),
            retry_after_ms: Some(500),
        }
    }
}

impl From<rusqlite::Error> for SpecError {
    fn from(error: rusqlite::Error) -> Self {
        if matches!(&error, rusqlite::Error::SqliteFailure(e, _) if matches!(e.code, rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked))
        {
            Self::busy()
        } else {
            Self::new("ERR_SPEC_DB", error.to_string())
        }
    }
}

impl From<std::io::Error> for SpecError {
    fn from(error: std::io::Error) -> Self {
        Self::new("ERR_IO", error.to_string())
    }
}

impl From<serde_json::Error> for SpecError {
    fn from(error: serde_json::Error) -> Self {
        Self::new("ERR_SPEC_DB_UNRECOGNIZED", error.to_string())
    }
}
