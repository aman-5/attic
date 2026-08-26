//! Attic — evidence-driven retrieval (Phase 4).
//!
//! Pipeline:
//! ```text
//! question → Query Router → QueryType → AnswerModePolicy
//!   → Query Evidence Contract → RetrievalPlanner → RetrievalPlan
//!   → candidate generators → fusion → evidence ranking
//!   → evidence validation → Evidence Manager (sufficiency)
//!       ├─ sufficient        → Context Builder → claims → verifier
//!       └─ insufficient      → targeted expansion / graph / source verification
//!                              → revalidate → sufficient | INSUFFICIENT_EVIDENCE
//! ```
//!
//! Critical principle: **retrieval proposes candidates. Evidence determines
//! what Attic is justified in saying.**
//!
//! Contracts implemented: `docs/contracts/evidence.md`,
//! `query_evidence.md`, `answer_modes.md`, `retrieval_plan.md`;
//! architecture decisions in `docs/decisions/ADR-012-phase4-retrieval.md`.

#![forbid(unsafe_code)]
#![deny(clippy::all)]

pub mod budget;
pub mod candidates;
pub mod claims;
pub mod context;
pub mod contract;
pub mod contradiction;
pub mod error;
pub mod fuse;
pub mod graph;
pub mod manager;
pub mod mode;
pub mod pipeline;
pub mod plan;
pub mod query;
pub mod rank;
pub mod validate;
pub mod verify;

pub use error::RetrievalError;
pub use mode::{AnswerMode, AnswerModePolicy};
pub use pipeline::{AnswerOutcome, AnswerRequest, RetrievalService};
pub use query::{Classification, QueryType, classify};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn taxonomy_covers_all_twelve_types() {
        assert_eq!(QueryType::all().len(), 12);
    }

    #[test]
    fn classifier_is_deterministic() {
        let a = classify("Where is Router defined in sable?").unwrap();
        let b = classify("Where is Router defined in sable?").unwrap();
        assert_eq!(a, b);
        assert_eq!(a.query_type, QueryType::DefinitionLookup);
    }

    #[test]
    fn malformed_queries_are_rejected_not_classified() {
        assert!(classify("").is_err());
        assert!(classify("   ").is_err());
        let long = "x".repeat(600);
        assert!(classify(&long).is_err());
        assert!(classify("\u{0}\u{1}\u{2}").is_err());
    }

    #[test]
    fn ambiguous_classification_stays_honest() {
        // "why does the configured port fail" hits configuration AND
        // debugging signals — must not silently claim certainty.
        let c = classify("why does the configured port fail?").unwrap();
        assert!(
            !c.competing_types.is_empty(),
            "overlapping signals should register competing types"
        );
    }
}
