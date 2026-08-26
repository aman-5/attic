//! Claims and deterministic answer-verification shapes (Phase 4 pipeline
//! stage "Answer/Claims → Answer Verifier").

use serde::{Deserialize, Serialize};

use crate::str_enum;

str_enum! {
    /// What kind of assertion a claim makes. The verifier checks claim-type-
    /// specific support rules.
    ClaimType {
        /// "X is defined at path:line".
        DefinitionLocation => "DEFINITION_LOCATION",
        /// "setting Y has value Z in path".
        ConfigurationValue => "CONFIGURATION_VALUE",
        /// Description of behavior grounded in implementation spans.
        BehaviorDescription => "BEHAVIOR_DESCRIPTION",
        /// "A imports/calls/extends B" — requires resolved relationships.
        RelationshipAssertion => "RELATIONSHIP_ASSERTION",
        /// Behavioral expectation grounded in test evidence.
        TestExpectation => "TEST_EXPECTATION",
        /// Documented intent grounded in knowledge evidence.
        KnowledgeStatement => "KNOWLEDGE_STATEMENT",
        /// Impact assessment grounded in callers/dependents.
        ImpactAssessment => "IMPACT_ASSESSMENT",
        /// Generic statement backed by lexical evidence only (LOW ceiling).
        General => "GENERAL",
    }
}

/// A single answer claim with its supporting evidence ids.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Claim {
    /// Claim text (no secret content; produced from redacted evidence).
    pub text: String,
    /// What kind of assertion this is.
    pub claim_type: ClaimType,
    /// Claim confidence in [0.0, 1.0].
    pub confidence: f64,
    /// Evidence ids that back this claim.
    pub evidence_ids: Vec<String>,
}

str_enum! {
    /// Deterministic verifier verdict for one claim.
    ClaimVerdict {
        /// Every support rule passed.
        Supported => "SUPPORTED",
        /// Support rules failed; claim must not be served as-is.
        Rejected => "REJECTED",
        /// Supported, but references contradicted evidence and must carry a
        /// contradiction disclosure.
        SupportedWithDisclosure => "SUPPORTED_WITH_DISCLOSURE",
    }
}

/// Result of verifying one claim against the validated evidence set.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct VerifiedClaim {
    /// The original claim.
    pub claim: Claim,
    /// Verdict.
    pub verdict: ClaimVerdict,
    /// Deterministic rejection/support reasons (inspectable, never opaque).
    pub reasons: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claim_type_round_trip() {
        for t in ClaimType::all() {
            let json = serde_json::to_string(t).unwrap();
            let back: ClaimType = serde_json::from_str(&json).unwrap();
            assert_eq!(t, &back);
        }
    }

    #[test]
    fn unknown_claim_type_rejected() {
        assert!(serde_json::from_str::<ClaimType>("\"NOPE\"").is_err());
    }
}
