//! Bounded graph/relationship expansion (Phase 4 §12).
//!
//! Graph traversal is EVIDENCE EXPANSION, not truth. Every traversed edge
//! keeps its relationship type, resolution level, confidence, provenance,
//! revision and freshness. Depth, node count and candidate budget are hard
//! caps — no unbounded traversal ever occurs.

use std::collections::VecDeque;

use attic_core::FreshnessState;
use attic_evidence::{
    AuthorityLevel, Evidence, EvidenceSourceType, RelationshipProvenance, ResolutionLevel,
    RetrievalSource,
};
use rusqlite::Connection;

use crate::budget::BudgetAccountant;
use crate::error::RetrievalError;

/// Seeds for a walk: entity ids (symbol occurrence ids or file occurrence
/// ids) with the repository they belong to.
pub struct GraphSeeds {
    pub repository_id: String,
    pub entity_ids: Vec<String>,
}

/// Expand relationships outward from `seeds` up to `max_depth` hops.
///
/// Deterministic BFS (edges ordered by id). INVALID edges are skipped;
/// STALE edges carry their freshness into the produced evidence so
/// validation can treat them per contract.
pub fn expand(
    conn: &Connection,
    seeds: &GraphSeeds,
    max_depth: u8,
    budget: &mut BudgetAccountant,
) -> Result<Vec<Evidence>, RetrievalError> {
    let mut out = Vec::new();
    let mut visited_nodes: std::collections::HashSet<String> =
        seeds.entity_ids.iter().cloned().collect();
    let mut visited_edges: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut queue: VecDeque<(String, u32)> = seeds
        .entity_ids
        .iter()
        .cloned()
        .map(|e| (e, 0u32))
        .collect();

    while let Some((entity_id, depth)) = queue.pop_front() {
        if depth >= max_depth as u32 {
            continue;
        }
        let edges = attic_storage::relationships_for_entity(conn, &entity_id, 32)?;
        // Deterministic order regardless of DB plan: sort by edge id.
        let mut edges = edges;
        edges.sort_by(|a, b| a.id.cmp(&b.id));
        for e in edges {
            if visited_edges.contains(&e.id) {
                continue;
            }
            visited_edges.insert(e.id.clone());

            if e.freshness_state == "INVALID" {
                continue;
            }
            // The far endpoint becomes the next node.
            let next = if e.source_entity_id == entity_id {
                e.target_entity_id.clone()
            } else {
                e.source_entity_id.clone()
            };
            if visited_nodes.contains(&next) && !next.starts_with("logical:") {
                continue;
            }

            // Node budget charges only REAL nodes; unresolved logical
            // targets are labels, not traversal nodes.
            if !next.starts_with("logical:") {
                if !budget.charge_graph_node() {
                    return Ok(out);
                }
                visited_nodes.insert(next.clone());
            }

            let resolution =
                ResolutionLevel::from_db_str(&e.resolution).unwrap_or(ResolutionLevel::Syntactic);
            let mut ev = Evidence::new(
                uuid::Uuid::new_v4().to_string(),
                seeds.repository_id.clone(),
            );
            ev.source_type = EvidenceSourceType::Relationship;
            ev.source_id = e.id.clone();
            ev.path = format!(
                "{} --{}({})--> {}",
                short(&e.source_entity_id),
                e.rel_type,
                resolution.as_str(),
                short(&next)
            );
            ev.source_revision_id = Some(e.source_revision_id.clone());
            ev.freshness_state =
                FreshnessState::from_db_str(&e.freshness_state).unwrap_or(FreshnessState::Unknown);
            ev.authority = AuthorityLevel::Derived;
            let conf = e.confidence.clamp(0.0, 0.99);
            ev.confidence = conf;
            ev.relationship_confidence = Some(conf);
            ev.relationship = Some(RelationshipProvenance {
                edge_id: e.id,
                rel_type: e.rel_type.clone(),
                resolution,
                confidence: conf,
                hop_depth: depth + 1,
            });
            ev.retrieval_sources.push(RetrievalSource {
                retriever_type: "GRAPH".to_owned(),
                score: conf,
                query_fragment: format!("hop {}", depth + 1),
            });
            ev.signals.freshness_score =
                Some(attic_evidence::signals::freshness_score(ev.freshness_state));
            ev.signals.relationship_confidence = Some(conf);
            ev.signals.structural_proximity = Some(match depth + 1 {
                1 => 0.9,
                2 => 0.6,
                _ => 0.3,
            });
            out.push(ev);

            if !next.starts_with("logical:") {
                queue.push_back((next, depth + 1));
            }
            if out.len() >= 128 || budget.graph_nodes_used >= budget.max_graph_nodes {
                return Ok(out);
            }
        }
    }
    Ok(out)
}

fn short(id: &str) -> String {
    if id.starts_with("logical:") {
        return id.to_owned();
    }
    id.chars().take(8).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_preserves_logical_ids() {
        assert_eq!(short("logical:abc123"), "logical:abc123");
        assert_eq!(short("0123456789abcdef").len(), 8);
    }
}
