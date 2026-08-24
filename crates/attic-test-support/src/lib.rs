//! attic-test-support — shared test helpers and fixtures (skeleton).
//!
//! This crate is used exclusively in `[dev-dependencies]` by other crates.
//! Implementation added as test infrastructure needs grow across phases.

#![forbid(unsafe_code)]
#![deny(clippy::all)]

/// Placeholder so the crate compiles during Bootstrap.
pub mod fixtures {
    /// Marker — replaced with real fixture builders in later phases.
    pub struct Fixtures;
}

#[cfg(test)]
mod tests {
    #[test]
    fn placeholder_passes() {
        // placeholder — replace with real assertions in Phase 1
    }
}
