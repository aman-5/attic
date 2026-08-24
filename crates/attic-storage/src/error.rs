//! Storage-layer error type.

use thiserror::Error;

/// All errors that can be produced by `attic-storage`.
#[derive(Debug, Error)]
pub enum StorageError {
    /// A rusqlite / SQLite error.
    #[error("SQLite error: {0}")]
    Sqlite(#[from] rusqlite::Error),

    /// A schema migration failed.
    #[error("migration error: {message}")]
    Migration {
        /// Human-readable description.
        message: String,
    },

    /// The writer queue is full and the caller should back off.
    #[error("writer queue full")]
    QueueFull,

    /// The writer queue worker has shut down.
    #[error("writer queue shut down")]
    QueueShutdown,

    /// A wrapped domain error from `attic-core`.
    #[error("domain error: {0}")]
    Domain(#[from] attic_core::CoreError),

    /// A JSON (de)serialisation error.
    #[error("JSON error: {0}")]
    Json(String),
}

impl From<serde_json::Error> for StorageError {
    fn from(e: serde_json::Error) -> Self {
        Self::Json(e.to_string())
    }
}
