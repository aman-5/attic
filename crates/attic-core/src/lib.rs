//! Attic — pure domain model.
//!
//! This crate contains only domain types: IDs, enumerations, and trait
//! signatures that are shared across all other Attic crates. It has no
//! dependency on async runtimes, databases, parsers, or network transports.
//!
//! # Bootstrap state
//! Skeleton only. Domain types are added starting in Phase 0 (contracts).

#![forbid(unsafe_code)]
#![deny(clippy::all)]

/// Placeholder module — domain types are introduced in Phase 0.
pub mod domain {}

#[cfg(test)]
mod tests {
    #[test]
    fn crate_is_present() {
        // placeholder — domain type tests added in Phase 0
    }
}
