//! Attic — pure domain model.
//!
//! This crate contains only domain types: IDs, enumerations, and trait
//! signatures that are shared across all other Attic crates.  It has no
//! dependency on async runtimes, databases, parsers, or network transports.
//!
//! # Bootstrap state
//! Currently a skeleton.  Types will be added incrementally starting in
//! Phase 0 (contracts) and Phase 1A (storage).

#![forbid(unsafe_code)]
#![deny(clippy::all)]

/// Placeholder for the Attic domain model.
///
/// Real types (SourceRevision, WorkspaceSnapshot, IndexGeneration, …) are
/// added in Phase 0 and beyond.  Having this module here ensures the crate
/// compiles and is testable from the workspace root.
pub mod domain {
    /// Opaque 64-bit row identifier used across Attic tables.
    ///
    /// This is a *physical* row id, not a semantic identity — see the
    /// canonical architecture §6 for the distinction.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
    pub struct RowId(pub i64);
}

#[cfg(test)]
mod tests {
    use super::domain::RowId;

    #[test]
    fn row_id_equality() {
        let a = RowId(1);
        let b = RowId(1);
        let c = RowId(2);
        assert_eq!(a, b);
        assert_ne!(a, c);
    }
}
