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

    /// A background OS thread could not be spawned.
    #[error("thread spawn failed: {0}")]
    ThreadSpawn(String),

    /// The mutation was part of a batch that was rolled back due to another
    /// mutation in the same batch failing.
    #[error("mutation rolled back: batch contained a failed mutation")]
    BatchRolledBack,

    /// All read connections in the pool are currently in use.
    #[error("read connection pool exhausted (max connections in use)")]
    PoolExhausted,

    /// A generic worker-level error (e.g., BEGIN/COMMIT failure).
    #[error("writer worker error: {0}")]
    Worker(String),

    /// An internal mutex was poisoned.
    #[error("internal mutex poisoned: {0}")]
    MutexPoisoned(String),

    /// The writer connection is in an unknown transactional state and has been
    /// permanently disabled.  All subsequent write attempts will be rejected
    /// until the storage layer is restarted.
    ///
    /// This is set when a `ROLLBACK` or `COMMIT` failure leaves the connection
    /// in a state that cannot be safely determined or recovered from.
    #[error(
        "writer connection poisoned (unrecoverable transaction finalization failure; restart required)"
    )]
    WriterPoisoned,
}

impl From<serde_json::Error> for StorageError {
    fn from(e: serde_json::Error) -> Self {
        Self::Json(e.to_string())
    }
}
