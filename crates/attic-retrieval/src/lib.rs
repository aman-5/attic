//! Attic — retrieval layer skeleton.
//!
//! Query classification, RetrievalPlanner, lexical/symbol/structural
//! retrievers, candidate fusion, and context builder live here.
//!
//! # Bootstrap state
//! Skeleton only.  Retrieval logic added in Phase 1D and Phase 4.

#![forbid(unsafe_code)]
#![deny(clippy::all)]

pub mod placeholder {
    pub fn retrieval_crate_present() -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::placeholder::retrieval_crate_present;

    #[test]
    fn crate_is_present() {
        assert!(retrieval_crate_present());
    }
}
