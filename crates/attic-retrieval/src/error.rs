//! Retrieval error type (Phase 4).

use attic_storage::StorageError;

/// Errors surfaced by the retrieval pipeline.
#[derive(Debug, thiserror::Error)]
pub enum RetrievalError {
    /// Storage read/write failure.
    #[error("storage error: {0}")]
    Storage(#[from] StorageError),
    /// Discovery / secure-source-access failure during verification.
    #[error("source access error: {0}")]
    Discovery(#[from] std::io::Error),
    /// Malformed or untrusted query input.
    #[error("invalid query input: {0}")]
    InvalidQuery(String),
    /// A policy invariant was violated (e.g. FAST mode attempted a
    /// filesystem read). Hard error, never silently skipped.
    #[error("policy violation ({field}): {detail}")]
    PolicyViolation {
        /// Violated budget/policy field name.
        field: &'static str,
        /// Human-readable detail.
        detail: String,
    },
    /// Serialization failure while persisting a plan.
    #[error("plan serialization error: {0}")]
    PlanJson(#[from] serde_json::Error),
}
