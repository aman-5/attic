//! Errors for the disposable semantic layer (Phase 5, ADR-013/ADR-014).
//!
//! Semantic failures are NEVER canonical-index failures: every variant is
//! recoverable by degrading to the non-semantic Phase 4 pipeline.

use thiserror::Error;

#[derive(Debug, Error)]
pub enum SemanticError {
    #[error("semantic store error: {0}")]
    Store(#[from] rusqlite::Error),

    #[error("provider '{provider}' is unavailable: {reason}")]
    ProviderUnavailable { provider: String, reason: String },

    #[error("embedding generation failed: {0}")]
    EmbeddingFailed(String),

    #[error("embedding batch cancelled after {completed} of {total} items")]
    Cancelled { completed: usize, total: usize },

    #[error("input exceeds provider maximum ({len} > {max} bytes)")]
    InputTooLarge { len: usize, max: usize },

    #[error("dimension mismatch: record has {record}, provider produces {expected}")]
    DimensionMismatch { record: usize, expected: usize },

    #[error("resource budget exhausted: {0}")]
    BudgetExhausted(String),

    #[error("canonical index read failed: {0}")]
    Canonical(String),
}

impl From<attic_storage::StorageError> for SemanticError {
    fn from(e: attic_storage::StorageError) -> Self {
        Self::Canonical(e.to_string())
    }
}
