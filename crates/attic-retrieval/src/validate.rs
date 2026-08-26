//! Evidence validation (Phase 4 §10): independently checks whether a
//! candidate can support the requested evidence requirement. Ranking asked
//! "likely useful?"; validation asks "can it support the requirement?".
//!
//! A high-ranked stale candidate is still stale. A high-ranked syntactic
//! relationship is not automatically a resolved relationship.

use attic_core::FreshnessState;
use attic_evidence::EvidenceSourceType as ST;
use attic_evidence::{
    AuthorityLevel, Evidence, EvidenceSourceType, ResolutionLevel, VerificationStatus,
};

use crate::contract::{FreshnessRequirement, QueryEvidenceContract};
use crate::plan::DropReason;

/// Outcome of validating one evidence item against the active contract.
#[derive(Debug, Clone, PartialEq)]
pub struct ValidationVerdict {
    /// Whether the item may count toward REQUIRED evidence satisfaction.
    pub counts_toward_required: bool,
    /// Whether the item may still appear in context (preferred/caveat).
    pub usable_for_context: bool,
    /// Deterministic rejection reason when fully rejected.
    pub drop_reason: Option<DropReason>,
    /// Human-readable explanation (inspectable; logged in plan steps).
    pub explanation: String,
}

impl ValidationVerdict {
    fn accept(expl: impl Into<String>) -> Self {
        Self {
            counts_toward_required: true,
            usable_for_context: true,
            drop_reason: None,
            explanation: expl.into(),
        }
    }

    fn context_only(expl: impl Into<String>) -> Self {
        Self {
            counts_toward_required: false,
            usable_for_context: true,
            drop_reason: None,
            explanation: expl.into(),
        }
    }

    fn reject(reason: DropReason, expl: impl Into<String>) -> Self {
        Self {
            counts_toward_required: false,
            usable_for_context: false,
            drop_reason: Some(reason),
            explanation: expl.into(),
        }
    }
}

/// Parse a stored span string `start_line:start_col-end_line:end_col`.
pub fn parse_span(s: &str) -> Option<attic_core::SourceSpan> {
    let mut parts = s.split(['-', ':']);
    let sl = parts.next()?.parse::<u32>().ok()?;
    let sc = parts.next()?.parse::<u32>().ok()?;
    let el = parts.next()?.parse::<u32>().ok()?;
    let ec = parts.next()?.parse::<u32>().ok()?;
    if el < sl || (el == sl && ec < sc) {
        return None;
    }
    Some(attic_core::SourceSpan::new(sl, sc, el, ec))
}

fn freshness_permits(state: FreshnessState, req: FreshnessRequirement) -> bool {
    match req {
        FreshnessRequirement::CurrentOnly => state == FreshnessState::Current,
        FreshnessRequirement::CurrentOrStale => matches!(
            state,
            FreshnessState::Current | FreshnessState::Stale | FreshnessState::PendingRefresh
        ),
        FreshnessRequirement::Any => state != FreshnessState::Invalid,
    }
}

/// Validate one evidence item.
pub fn validate(ev: &Evidence, contract: &QueryEvidenceContract) -> ValidationVerdict {
    // 1. Provenance: revision + generation must exist (invariant 1 / EV-02).
    if ev.source_revision_id.is_none() || ev.source_id.is_empty() {
        return ValidationVerdict::reject(
            DropReason::ProvenanceInvalid,
            "missing source_revision_id or source_id",
        );
    }
    if ev.freshness_state == FreshnessState::Invalid {
        return ValidationVerdict::reject(DropReason::StaleBeyondThreshold, "INVALID freshness");
    }

    // 2. Span validity where present.
    if let Some(span) = &ev.source_span
        && span.end_line < span.start_line
    {
        return ValidationVerdict::reject(DropReason::SpanInvalid, "inverted span");
    }

    // 3. Relationship-specific rules: confidence floor + resolution honesty.
    if ev.source_type == ST::Relationship {
        let rel_conf = ev.relationship_confidence.unwrap_or(0.0);
        let min = contract.relationship_confidence_min.unwrap_or(0.0);
        if rel_conf < min {
            return ValidationVerdict::reject(
                DropReason::RelationshipConfidenceTooLow,
                format!("relationship confidence {rel_conf:.2} below required {min:.2}"),
            );
        }
        let unresolved_syntactic = ev
            .relationship
            .as_ref()
            .is_some_and(|r| r.resolution == ResolutionLevel::Syntactic);
        if unresolved_syntactic {
            return ValidationVerdict::context_only(
                "syntactic relationship never counts as a resolved fact",
            );
        }
        if rel_conf >= 1.0 {
            return ValidationVerdict::reject(
                DropReason::ProvenanceInvalid,
                "relationship confidence must stay below 1.0",
            );
        }
        return ValidationVerdict::accept("resolved relationship above confidence floor");
    }

    // 4. Freshness vs contract for non-relationship evidence.
    if !freshness_permits(ev.freshness_state, contract.freshness_requirement) {
        // CURRENT_ONLY with STALE evidence: not counted toward requirements.
        // It stays context-eligible only if verification later upgrades it —
        // handled by the manager's source-verification strategy.
        return ValidationVerdict::context_only(format!(
            "freshness {} below requirement {:?}",
            ev.freshness_state.as_str(),
            contract.freshness_requirement
        ));
    }

    // 5. Authority applicability: at least ONE requirement accepts this type
    //    (required or preferred); otherwise context-only.
    let accepted_by = |reqs: &[crate::contract::EvidenceRequirement]| {
        reqs.iter()
            .any(|r| r.source_types.contains(&ev.source_type))
    };
    if !accepted_by(&contract.required_evidence) && !accepted_by(&contract.preferred_evidence) {
        if contract.query_type == crate::query::QueryType::GenericSearch {
            return ValidationVerdict::accept("generic search accepts any evidence");
        }
        return ValidationVerdict::context_only("source type outside contract scope");
    }

    ValidationVerdict::accept("provenance, freshness and authority satisfied")
}

/// Whether an evidence item satisfies one named requirement slot.
pub fn satisfies_requirement(
    ev: &Evidence,
    req: &crate::contract::EvidenceRequirement,
    contract: &QueryEvidenceContract,
) -> bool {
    if !req.source_types.contains(&ev.source_type) {
        return false;
    }
    if ev.source_type == ST::Relationship {
        let conf = ev.relationship_confidence.unwrap_or(0.0);
        if conf < contract.relationship_confidence_min.unwrap_or(0.0) {
            return false;
        }
        // SYNTACTIC edges never satisfy requirements (honesty ladder).
        if ev
            .relationship
            .as_ref()
            .is_some_and(|r| r.resolution == ResolutionLevel::Syntactic)
        {
            return false;
        }
    }
    match contract.freshness_requirement {
        FreshnessRequirement::CurrentOnly => ev.freshness_state == FreshnessState::Current,
        FreshnessRequirement::CurrentOrStale => {
            matches!(
                ev.freshness_state,
                FreshnessState::Current | FreshnessState::Stale | FreshnessState::PendingRefresh
            ) && ev.verification_state != VerificationStatus::Contradicted
        }
        FreshnessRequirement::Any => ev.verification_state != VerificationStatus::Contradicted,
    }
}

/// Source types acceptable for an authority label — used by the claim
/// verifier to check that evidence can support a claim TYPE.
pub fn supports_claim_type(
    ct: attic_evidence::ClaimType,
    st: EvidenceSourceType,
    authority: AuthorityLevel,
) -> bool {
    use attic_evidence::ClaimType as C;
    match ct {
        C::DefinitionLocation => matches!(st, ST::SourceCode | ST::GeneratedSource),
        C::ConfigurationValue => st == ST::Configuration || authority == AuthorityLevel::Configured,
        C::BehaviorDescription => st == ST::SourceCode,
        C::RelationshipAssertion => st == ST::Relationship,
        C::TestExpectation => st == ST::Test,
        C::KnowledgeStatement => matches!(st, ST::Knowledge | ST::Documentation),
        C::ImpactAssessment => matches!(st, ST::Relationship | ST::SourceCode | ST::Test),
        C::General => true,
    }
}
