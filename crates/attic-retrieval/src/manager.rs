//! Evidence Manager (Phase 4 §11): sufficiency evaluation against the Query
//! Evidence Contract plus the bounded targeted-expansion loop.
//!
//! ```text
//! validated evidence → satisfies contract?
//!   yes → context
//!   no  → targeted expansion / graph expansion / source verification
//!         → revalidate
//!         → sufficient | INSUFFICIENT_EVIDENCE (never a fabricated answer)
//! ```

use attic_evidence::{Contradiction, Evidence, VerificationStatus};

use crate::budget::BudgetAccountant;
use crate::contract::{FallbackStrategy, QueryEvidenceContract};
use crate::graph::{GraphSeeds, expand};

/// Which contract requirement slots are satisfied by which evidence ids.
#[derive(Debug, Default)]
pub struct SufficiencyReport {
    /// (requirement label, satisfying evidence ids) — deterministic order.
    pub satisfied: Vec<(String, Vec<String>)>,
    /// Unsatisfied required labels.
    pub unsatisfied: Vec<String>,
    pub sufficient: bool,
}

impl SufficiencyReport {
    /// Evaluate the contract against validated evidence.
    pub fn evaluate(contract: &QueryEvidenceContract, evidence: &[Evidence]) -> Self {
        let mut satisfied = Vec::new();
        let mut unsatisfied = Vec::new();
        for req in &contract.required_evidence {
            let hits: Vec<String> = evidence
                .iter()
                .filter(|ev| crate::validate::satisfies_requirement(ev, req, contract))
                .map(|ev| ev.id.clone())
                .collect();
            if hits.len() >= req.min_count as usize {
                satisfied.push((req.evidence_type.clone(), hits));
            } else {
                unsatisfied.push(req.evidence_type.clone());
            }
        }
        Self {
            sufficient: unsatisfied.is_empty(),
            satisfied,
            unsatisfied,
        }
    }
}

/// One targeted-expansion round's inputs, produced by strategy selection.
pub enum ExpansionAction {
    /// Re-run lexical generation with broader terms.
    BroaderFts(Vec<String>),
    /// Bounded graph walk from these seeds.
    BoundedGraph(GraphSeeds),
    /// Verify top stale/unverified candidates against live source.
    SourceVerification(Vec<String>),
    /// Knowledge lookup with the same terms.
    KnowledgeLookup(Vec<String>),
}

/// Choose the next expansion action given remaining fallbacks.
///
/// Deterministic: when a CURRENT_ONLY contract holds stale/unverified
/// candidates, SOURCE VERIFICATION is attempted FIRST regardless of declared
/// order — it is the cheapest route back to a satisfiable state and every
/// other expansion re-discovers the same stale rows. Otherwise strategies
/// fire in the contract's declared order; each at most once.
pub fn next_expansion(
    contract: &QueryEvidenceContract,
    used: &[FallbackStrategy],
    evidence: &[Evidence],
) -> Option<ExpansionAction> {
    let stale_present = contract.freshness_requirement
        == crate::contract::FreshnessRequirement::CurrentOnly
        && evidence.iter().any(|ev| {
            ev.source_type != attic_evidence::EvidenceSourceType::Relationship
                && (ev.freshness_state != attic_core::FreshnessState::Current
                    || ev.verification_state == VerificationStatus::Unverified)
        });
    if stale_present && !used.contains(&FallbackStrategy::SourceVerification) {
        let targets: Vec<String> = evidence
            .iter()
            .filter(|ev| {
                ev.source_type != attic_evidence::EvidenceSourceType::Relationship
                    && (ev.freshness_state != attic_core::FreshnessState::Current
                        || ev.verification_state == VerificationStatus::Unverified)
            })
            .take(5)
            .map(|ev| ev.id.clone())
            .collect();
        if !targets.is_empty() {
            return Some(ExpansionAction::SourceVerification(targets));
        }
    }

    for f in &contract.allowed_fallbacks {
        if used.contains(f) {
            continue;
        }
        match f {
            FallbackStrategy::BroaderFts => {
                return Some(ExpansionAction::BroaderFts(broader_terms(evidence)));
            }
            FallbackStrategy::BoundedGraph => {
                let seeds = seed_entities(evidence);
                if !seeds.entity_ids.is_empty() {
                    return Some(ExpansionAction::BoundedGraph(seeds));
                }
            }
            FallbackStrategy::SourceVerification => {
                let targets: Vec<String> = evidence
                    .iter()
                    .filter(|ev| {
                        ev.source_type != attic_evidence::EvidenceSourceType::Relationship
                            && matches!(
                                ev.verification_state,
                                VerificationStatus::Unverified | VerificationStatus::Stale
                            )
                            || ev.freshness_state != attic_core::FreshnessState::Current
                    })
                    .take(5)
                    .map(|ev| ev.id.clone())
                    .collect();
                if !targets.is_empty() {
                    return Some(ExpansionAction::SourceVerification(targets));
                }
            }
            FallbackStrategy::KnowledgeLookup => {
                return Some(ExpansionAction::KnowledgeLookup(knowledge_terms(evidence)));
            }
            FallbackStrategy::SemanticSearch => {
                // Phase 5 capability: recorded as unavailable, never fake.
                continue;
            }
        }
    }
    None
}

/// Widen lexical terms using tokens harvested from top evidence snippets.
fn broader_terms(evidence: &[Evidence]) -> Vec<String> {
    let mut terms: Vec<String> = Vec::new();
    for ev in evidence.iter().take(5) {
        if let Some(snip) = &ev.snippet {
            for word in snip.split_whitespace().take(6) {
                let w: String = word
                    .chars()
                    .filter(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect();
                if w.len() >= 4 && !terms.contains(&w.to_lowercase()) {
                    terms.push(w.to_lowercase());
                }
                if terms.len() >= 4 {
                    return terms;
                }
            }
        }
    }
    terms
}

fn knowledge_terms(evidence: &[Evidence]) -> Vec<String> {
    broader_terms(evidence)
}

/// Seed entity ids for graph expansion: relationship-adjacent symbol or
/// file occurrence ids from current evidence.
fn seed_entities(evidence: &[Evidence]) -> GraphSeeds {
    let repo = evidence
        .first()
        .map(|e| e.repository_id.clone())
        .unwrap_or_default();
    let mut ids: Vec<String> = Vec::new();
    for ev in evidence {
        // Symbol/file occurrence ids are the evidence source_id; edges are
        // keyed by those entities.
        if ev.source_type != attic_evidence::EvidenceSourceType::Relationship
            && !ids.contains(&ev.source_id)
        {
            ids.push(ev.source_id.clone());
        }
        if ids.len() >= 6 {
            break;
        }
    }
    GraphSeeds {
        repository_id: repo,
        entity_ids: ids,
    }
}

/// Run one bounded graph expansion round.
pub fn run_graph_expansion(
    conn: &rusqlite::Connection,
    contract: &QueryEvidenceContract,
    policy_max_depth: u8,
    seeds: GraphSeeds,
    budget: &mut BudgetAccountant,
) -> Result<Vec<Evidence>, crate::error::RetrievalError> {
    // Contract expansion may add up to 2 hops beyond the plan depth cap;
    // never more than the policy allows overall.
    let depth = policy_max_depth
        .min(policy_max_depth.saturating_add(0))
        .max(1);
    let _ = contract;
    expand(conn, &seeds, depth, budget)
}

/// Mark contradicted pairs on both sides after detection. Both items remain
/// surfaced (invariant 6) but carry CONTRADICTED verification state so
/// sufficiency counting can exclude them where contracts demand.
pub fn apply_contradictions(evidence: &mut [Evidence], contradictions: &[Contradiction]) {
    for c in contradictions {
        for ev in evidence.iter_mut() {
            if ev.id == c.evidence_a || ev.id == c.evidence_b {
                if ev.verification_state != VerificationStatus::Verified {
                    ev.verification_state = VerificationStatus::Contradicted;
                } else {
                    // Verified facts keep their state but must be disclosed;
                    // record via a marker contradiction id in retrieval sources.
                    ev.retrieval_sources.push(attic_evidence::RetrievalSource {
                        retriever_type: "CONTRADICTION_DISCLOSURE".to_owned(),
                        score: 0.0,
                        query_fragment: format!(
                            "conflicts with {}",
                            if ev.id == c.evidence_a {
                                &c.evidence_b
                            } else {
                                &c.evidence_a
                            }
                        ),
                    });
                }
            }
        }
    }
}
