//! Claim derivation and the deterministic Answer Verifier (Phase 4 §16).
//!
//! Claims map to evidence ids; verification checks evidence existence,
//! claim-type support, span validity, freshness, relationship
//! resolution/confidence, and contradiction disclosure. A second LLM
//! verifier is NOT required in V1 — this module is fully deterministic.

use attic_core::FreshnessState;
use attic_evidence::{
    Claim, ClaimType, ClaimVerdict, Contradiction, Evidence, EvidenceSourceType as ST,
    ResolutionLevel, VerifiedClaim,
};

use crate::contract::FreshnessRequirement;
use crate::query::QueryType;

/// Maximum claims derived per answer (bounded output).
pub const MAX_CLAIMS: u32 = 12;

/// Deterministically derive candidate claims from validated evidence for a
/// query type. Derivation never invents facts: each claim quotes only what
/// the evidence snippet/path/span contain.
pub fn derive_claims(
    qt: QueryType,
    evidence: &[Evidence],
    _contradictions: &[Contradiction],
) -> Vec<Claim> {
    let mut claims: Vec<Claim> = Vec::new();

    let push = |claims: &mut Vec<Claim>, c: Claim| {
        if claims.len() < MAX_CLAIMS as usize && !claims.iter().any(|x| x.text == c.text) {
            claims.push(c);
        }
    };

    for ev in evidence {
        let conf = ev.signals.combined_score.unwrap_or(0.0);
        match ev.source_type {
            ST::SourceCode | ST::GeneratedSource => {
                // Definition-location style claims when a symbol signature
                // exists; behavior claims otherwise.
                let sig = ev.snippet.as_deref().unwrap_or("").trim();
                if !sig.is_empty() {
                    if qt == QueryType::DefinitionLookup || qt == QueryType::ExactLookup {
                        push(
                            &mut claims,
                            Claim {
                                text: format!(
                                    "`{}` is defined in `{}` at {}.",
                                    first_token(sig),
                                    ev.path,
                                    span_label(ev)
                                ),
                                claim_type: ClaimType::DefinitionLocation,
                                confidence: conf,
                                evidence_ids: vec![ev.id.clone()],
                            },
                        );
                    } else {
                        push(
                            &mut claims,
                            Claim {
                                text: format!(
                                    "Implementation in `{}` ({}) contains: {}",
                                    ev.path,
                                    span_label(ev),
                                    first_line(sig)
                                ),
                                claim_type: ClaimType::BehaviorDescription,
                                confidence: conf,
                                evidence_ids: vec![ev.id.clone()],
                            },
                        );
                    }
                }
            }
            ST::Configuration => {
                if let Some((key, value)) = contradiction_config_probe(ev) {
                    push(
                        &mut claims,
                        Claim {
                            text: format!("`{key}` is set to `{value}` in `{}`.", ev.path),
                            claim_type: ClaimType::ConfigurationValue,
                            confidence: conf,
                            evidence_ids: vec![ev.id.clone()],
                        },
                    );
                }
            }
            ST::Test => {
                let sig = ev.snippet.as_deref().unwrap_or("").trim();
                if !sig.is_empty() {
                    push(
                        &mut claims,
                        Claim {
                            text: format!(
                                "Test expectations in `{}` include: {}",
                                ev.path,
                                first_line(sig)
                            ),
                            claim_type: ClaimType::TestExpectation,
                            confidence: conf,
                            evidence_ids: vec![ev.id.clone()],
                        },
                    );
                }
            }
            ST::Knowledge | ST::Documentation => {
                let sig = ev.snippet.as_deref().unwrap_or("").trim();
                if !sig.is_empty() {
                    push(
                        &mut claims,
                        Claim {
                            text: format!(
                                "Project documentation (`{}`) states: {}",
                                ev.path,
                                first_line(sig)
                            ),
                            claim_type: ClaimType::KnowledgeStatement,
                            confidence: conf * 0.9,
                            evidence_ids: vec![ev.id.clone()],
                        },
                    );
                }
            }
            ST::Relationship => {
                if let Some(rel) = &ev.relationship
                    && matches!(
                        rel.resolution,
                        ResolutionLevel::PackageResolved
                            | ResolutionLevel::SymbolResolved
                            | ResolutionLevel::BuildResolved
                            | ResolutionLevel::FrameworkResolved
                    )
                {
                    push(
                        &mut claims,
                        Claim {
                            text: format!(
                                "Relationship {}: {} at hop {}.",
                                rel.rel_type, ev.path, rel.hop_depth
                            ),
                            claim_type: ClaimType::RelationshipAssertion,
                            confidence: rel.confidence,
                            evidence_ids: vec![ev.id.clone()],
                        },
                    );
                }
            }
        }
    }

    // Impact analysis gets explicit impact claims from relationship chains.
    if qt == QueryType::ImpactAnalysis {
        let related: Vec<&Evidence> = evidence
            .iter()
            .filter(|e| e.source_type == ST::Relationship)
            .take(3)
            .collect();
        if !related.is_empty() {
            let ids: Vec<String> = related.iter().map(|e| e.id.clone()).collect();
            claims.insert(
                0,
                Claim {
                    text: format!(
                        "Change impact reaches {} downstream entities via resolved relationships.",
                        related.len()
                    ),
                    claim_type: ClaimType::ImpactAssessment,
                    confidence: related
                        .iter()
                        .map(|e| e.relationship_confidence.unwrap_or(0.0))
                        .fold(0.0f64, f64::max),
                    evidence_ids: ids,
                },
            );
        }
    }

    claims
}

fn first_token(s: &str) -> String {
    s.split_whitespace()
        .next()
        .unwrap_or("")
        .trim_matches(|c: char| c.is_ascii_punctuation())
        .to_owned()
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").chars().take(160).collect()
}

fn span_label(ev: &Evidence) -> String {
    ev.source_span
        .map(|s| format!("line {}", s.start_line + 1))
        .unwrap_or_else(|| "unknown line".into())
}

/// Re-export probe used by claim derivation (keeps contradiction helpers
/// internal to that module).
pub(crate) fn contradiction_config_probe(ev: &Evidence) -> Option<(String, String)> {
    let snip = ev.snippet.as_ref()?;
    for line in snip.lines() {
        let line = line.trim();
        let sep = line.find(['=', ':'])?;
        let (before, after) = line.split_at(sep);
        let key = before.trim();
        let value = after[1..].trim().trim_matches(['"', '\'', ',']);
        if !key.is_empty() && !value.is_empty() {
            return Some((key.to_owned(), value.to_owned()));
        }
    }
    None
}

/// Verify claims deterministically. Returns per-claim verdicts; unsupported
/// claims are REJECTED (never silently repaired or served).
pub struct VerifyConfig {
    /// Freshness floor from the active contract.
    pub freshness_requirement: FreshnessRequirement,
    /// Minimum confidence required for RELATIONSHIP_ASSERTION.
    pub relationship_confidence_min: f64,
}

pub fn verify_claims(
    claims: Vec<Claim>,
    evidence: &[Evidence],
    contradictions: &[Contradiction],
    cfg: &VerifyConfig,
) -> Vec<VerifiedClaim> {
    let contradicted: std::collections::HashSet<&str> = contradictions
        .iter()
        .flat_map(|c| [c.evidence_a.as_str(), c.evidence_b.as_str()])
        .collect();

    claims
        .into_iter()
        .map(|claim| {
            let mut reasons = Vec::new();

            // 1. Every referenced evidence id must exist.
            let mut backing: Vec<&Evidence> = Vec::new();
            for id in &claim.evidence_ids {
                match evidence.iter().find(|e| &e.id == id) {
                    Some(e) => backing.push(e),
                    None => reasons.push(format!("evidence {id} does not exist")),
                }
            }
            if backing.is_empty() {
                reasons.push("no backing evidence".to_owned());
                return VerifiedClaim { claim, verdict: ClaimVerdict::Rejected, reasons };
            }

            // 2. Claim-type support rules.
            for ev in &backing {
                if !crate::validate::supports_claim_type(claim.claim_type, ev.source_type, ev.authority)
                {
                    reasons.push(format!(
                        "evidence {} ({}) cannot support {:?}",
                        ev.id,
                        ev.source_type.as_str(),
                        claim.claim_type
                    ));
                }
            }

            // 3. Span validity on source-backed claims.
            for ev in &backing {
                if let Some(span) = ev.source_span
                    && span.end_line < span.start_line
                {
                    reasons.push(format!("evidence {} has invalid span", ev.id));
                }
            }

            // 4. Freshness consistency with the contract. Live-source
            // verified facts satisfy CURRENT_ONLY without the indexed
            // artifact's freshness being rewritten (ADR-012 D3).
            match cfg.freshness_requirement {
                FreshnessRequirement::CurrentOnly => {
                    if backing.iter().all(|e| {
                        e.freshness_state != FreshnessState::Current && !e.live_source_verified
                    }) {
                        reasons.push(
                            "all backing evidence is below CURRENT_ONLY freshness".to_owned(),
                        );
                    }
                }
                FreshnessRequirement::CurrentOrStale | FreshnessRequirement::Any => {}
            }

            // 5. Relationship claims meet resolution/confidence floors.
            if claim.claim_type == ClaimType::RelationshipAssertion {
                let ok = backing.iter().any(|e| {
                    e.source_type == ST::Relationship
                        && e.relationship_confidence.unwrap_or(0.0) >= cfg.relationship_confidence_min
                        && e.relationship.as_ref().is_some_and(|r| {
                            matches!(
                                r.resolution,
                                ResolutionLevel::SymbolResolved
                                    | ResolutionLevel::PackageResolved
                                    | ResolutionLevel::BuildResolved
                                    | ResolutionLevel::FrameworkResolved
                            )
                        })
                });
                if !ok {
                    reasons.push(
                        "relationship assertion lacks a sufficiently-resolved edge".to_owned(),
                    );
                }
            }

            // 6. Contradiction disclosure.
            let touches_contradiction = backing
                .iter()
                .any(|e| contradicted.contains(e.id.as_str()))
                || backing.iter().any(|e| {
                    e.retrieval_sources
                        .iter()
                        .any(|s| s.retriever_type == "CONTRADICTION_DISCLOSURE")
                });

            if reasons.is_empty() {
                if touches_contradiction {
                    VerifiedClaim {
                        verdict: ClaimVerdict::SupportedWithDisclosure,
                        reasons: vec!["backing evidence participates in a detected contradiction; disclosure required".to_owned()],
                        claim,
                    }
                } else {
                    VerifiedClaim {
                        verdict: ClaimVerdict::Supported,
                        reasons,
                        claim,
                    }
                }
            } else {
                VerifiedClaim {
                    claim,
                    verdict: ClaimVerdict::Rejected,
                    reasons,
                }
            }
        })
        .collect()
}
