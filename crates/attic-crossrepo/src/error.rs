//! Error type for cross-repository operations.

/// Failures surfaced by `attic-crossrepo`.
#[derive(Debug, thiserror::Error)]
pub enum CrossRepoError {
    /// Storage layer failure.
    #[error("storage error: {0}")]
    Storage(#[from] attic_storage::StorageError),

    /// SQLite failure outside the storage abstraction.
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),

    /// Repository root escaped validation or does not exist.
    #[error("invalid repository root: {0}")]
    InvalidRoot(String),

    /// A manifest path escaped its repository boundary (never expected —
    /// paths are repo-relative and validated before use).
    #[error("path escaped repository boundary: {0}")]
    PathEscape(String),

    /// Unknown TEXT variant encountered in the database.
    #[error("unknown {type_name} variant: {value}")]
    UnknownVariant {
        /// Enum name for the message.
        type_name: &'static str,
        /// Offending stored value.
        value: String,
    },

    /// Input exceeded a documented bound; the operation is refused rather
    /// than silently truncated.
    #[error("input exceeds limit {limit}: {context}")]
    LimitExceeded {
        /// Which limit was hit.
        limit: &'static str,
        /// Contextual detail.
        context: String,
    },

    /// No authoritative SourceRevision exists for the repository.
    /// Cross-repo catalog/edges require a real Phase 1B/2 source revision.
    #[error("no source revision for repository {repository_id} — index first")]
    NoSourceRevision {
        /// The repository that lacks a source revision.
        repository_id: String,
    },
}
