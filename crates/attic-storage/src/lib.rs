//! Attic — storage layer skeleton.
//!
//! SQLite connection management, schema migrations, DAOs, FTS5 helpers,
//! and the bounded DB-writer queue will live here.
//!
//! # Bootstrap state
//! Skeleton only.  The SQLite binding is chosen and added in Phase 1A.

#![forbid(unsafe_code)]
#![deny(clippy::all)]

/// Placeholder module.  Real storage implementations are added in Phase 1A.
pub mod placeholder {
    /// Bootstrap placeholder — removed when Phase 1A begins.
    pub fn storage_crate_present() -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::placeholder::storage_crate_present;

    #[test]
    fn crate_is_present() {
        assert!(storage_crate_present());
    }
}
