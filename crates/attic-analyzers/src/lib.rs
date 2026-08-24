//! Attic — analyzer registry skeleton.
//!
//! Contains the `Analyzer` trait, `AnalyzerRegistry`, `GenericAnalyzer`,
//! and language/format-specific analyzers.
//!
//! # Bootstrap state
//! Skeleton only.  Analyzer trait and GenericAnalyzer added in Phase 1C.

#![forbid(unsafe_code)]
#![deny(clippy::all)]

pub mod placeholder {
    pub fn analyzers_crate_present() -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::placeholder::analyzers_crate_present;

    #[test]
    fn crate_is_present() {
        assert!(analyzers_crate_present());
    }
}
