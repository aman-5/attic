//! Attic — indexing orchestration skeleton.
//!
//! Orchestrates SourceRevision capture, indexing task scheduling,
//! invalidation DAG, freshness transitions, and checkpoints.
//!
//! # Bootstrap state
//! Skeleton only.  Orchestration added in Phase 1A–1C.

#![forbid(unsafe_code)]
#![deny(clippy::all)]

pub mod placeholder {
    pub fn indexing_crate_present() -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::placeholder::indexing_crate_present;

    #[test]
    fn crate_is_present() {
        assert!(indexing_crate_present());
    }
}
