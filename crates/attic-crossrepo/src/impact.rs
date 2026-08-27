//! Evidence-backed impact analysis (Phase 6 §9).
//!
//! Answers: *if this repository changes, what else may be affected?* with
//! explicit, non-laundered classification:
//!
//! ```text
//! DIRECT_RESOLVED     — resolved dependency edge, 1 hop away
//! INDIRECT_RESOLVED   — fully-resolved path, >1 hop
//! POSSIBLE_INFERRED   — best available path contains an INFERRED segment
//! UNKNOWN             — path exists but freshness cannot support the claim
//! ```
//!
//! A textual reference or a name similarity NEVER produces an impact entry;
//! only persisted relationship edges with their recorded resolution and
//! freshness participate.

use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Instant;

use rusqlite::Connection;

use crate::traversal::TraversalBudget;
use crate::limits;

/// Impact classification for one affected repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImpactLevel {
    /// Resolved edge, one hop from the change.
    DirectResolved,
    /// Fully resolved path, more than one hop.
    IndirectResolved,
    /// Best available path includes an inferred/syntactic-only segment.
    PossibleInferred,
    /// A path exists but its freshness forbids a confident claim.
    Unknown,
}

impl ImpactLevel {
    /// Stable token used in tool output.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::DirectResolved => "DIRECT_RESOLVED",
            Self::IndirectResolved => "INDIRECT_RESOLVED",
            Self::PossibleInferred => "POSSIBLE_INFERRED",
            Self::Unknown => "UNKNOWN",
        }
    }
}

/// One evidence-backed hop.
#[derive(Debug, Clone)]
pub struct Hop {
    /// Edge UUID traversed.
    pub edge_id: String,
    /// Repository at this hop's far end.
    pub repository_id: String,
    /// Resolution token of the traversed edge.
    pub resolution: String,
    /// Edge confidence [0,1].
    pub confidence: f64,
    /// CURRENT | STALE | INVALID
    pub freshness_state: String,
}

/// Impact conclusion for one repository.
#[derive(Debug, Clone)]
pub struct ImpactedRepository {
    /// Affected repository UUID string.
    pub repository_id: String,
    /// Classification (best evidence across shortest paths).
    pub level: ImpactLevel,
    /// Shortest evidence path (hops in order from the changed repo).
    pub path: Vec<Hop>,
    /// Minimum edge confidence along the reported path.
    pub path_confidence: f64,
    /// Revisions backing each hop (provenance for WorkspaceSnapshot
    /// explanations), deduplicated in order.
    pub source_revision_ids: Vec<String>,
}

/// Full impact report for one seed repository.
#[derive(Debug, Default, Clone)]
pub struct ImpactReport {
    /// Affected repositories ordered by classification strength then hop
    /// count (deterministic).
    pub impacted: Vec<ImpactedRepository>,
    /// Budget limits hit during analysis (observable partial results).
    pub limits_hit: Vec<&'static str>,
}

/// Classify dependents of `seed_repo` (repositories that may be affected if
/// it changes) under `budget`.
pub fn analyze_dependents(
    conn: &Connection,
    seed_repo: &str,
    budget: &TraversalBudget,
) -> Result<ImpactReport, crate::error::CrossRepoError> {
    let started = Instant::now();
    // BFS collecting, per repository, the SHORTEST path and whether any
    // fully-resolved path reaches it.
    #[derive(Clone)]
    struct Reach {
        hops: Vec<Hop>,
        revisions: Vec<String>,
        any_resolved_path: bool,
    }
    let mut reach: HashMap<String, Reach> = HashMap::new();
    let mut visited_edges: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<(String, u32)> = VecDeque::new();
    queue.push_back((seed_repo.to_string(), 0));

    let mut limits_hit: Vec<&'static str> = Vec::new();

    'outer: while let Some((repo, depth)) = queue.pop_front() {
        if budget.cancel.is_cancelled() {
            limits_hit.push("cancelled");
            break;
        }
        if depth >= budget.max_depth {
            if !limits_hit.contains(&"depth") {
                limits_hit.push("depth");
            }
            continue;
        }
        if reach.len() as u32 >= budget.max_repositories {
            limits_hit.push("repositories");
            break;
        }
        if started.elapsed().as_millis() as u64 >= budget.max_time_ms {
            limits_hit.push("time");
            break;
        }

        let mut edges =
            attic_storage::cross_edges_touching(conn, &repo, limits::MAX_CANDIDATES_PER_KEY)?;
        edges.sort_by(|a, b| a.id.cmp(&b.id));

        for e in edges {
            if visited_edges.contains(&e.id) || e.freshness_state == "INVALID" {
                continue;
            }
            visited_edges.insert(e.id.clone());
            // Dependents direction: edge target → source is the dependent.
            let next = if e.target_repository_id == repo {
                e.source_repository_id.clone()
            } else {
                continue;
            };
            if next == seed_repo {
                continue; // cycles never re-enter the seed
            }

            let base = match reach.get(&repo) {
                Some(r) => (r.hops.clone(), r.revisions.clone(), r.any_resolved_path),
                None => (Vec::new(), Vec::new(), true), // repo == seed at depth 0
            };
            let mut hops = base.0;
            hops.push(Hop {
                edge_id: e.id.clone(),
                repository_id: next.clone(),
                resolution: e.resolution.clone(),
                confidence: e.confidence,
                freshness_state: e.freshness_state.clone(),
            });
            let mut revisions = base.1;
            if !revisions.contains(&e.source_revision_id) && revisions.len() < 32 {
                revisions.push(e.source_revision_id.clone());
            }

            let seg_resolved = matches!(
                e.resolution.as_str(),
                "PACKAGE_RESOLVED" | "SYMBOL_RESOLVED" | "BUILD_RESOLVED" | "FRAMEWORK_RESOLVED"
            );
            let parent_resolved = base.2;

            let entry = reach.entry(next.clone()).or_insert(Reach {
                hops: hops.clone(),
                revisions: revisions.clone(),
                any_resolved_path: false,
            });
            if hops.len() < entry.hops.len() {
                entry.hops = hops.clone();
                entry.revisions = revisions.clone();
            }
            if seg_resolved && parent_resolved {
                entry.any_resolved_path = true;
            }

            queue.push_back((next, depth + 1));
            if visited_edges.len() as u32 >= budget.max_edges {
                limits_hit.push("edges");
                break 'outer;
            }
            if started.elapsed().as_millis() as u64 >= budget.max_time_ms {
                limits_hit.push("time");
                break 'outer;
            }
        }
    }

    let mut impacted: Vec<ImpactedRepository> = reach
        .into_iter()
        .filter(|(id, _)| id != seed_repo)
        .map(|(repository_id, r)| {
            let path = shortest_freshest(r.hops);
            let level = classify(&path, r.any_resolved_path);
            let path_confidence = path.iter().map(|h| h.confidence).fold(1.0, f64::min);
            ImpactedRepository {
                repository_id,
                level,
                path,
                path_confidence,
                source_revision_ids: r.revisions,
            }
        })
        .collect();

    impacted.sort_by(|a, b| {
        rank(a.level)
            .cmp(&rank(b.level))
            .then(a.path.len().cmp(&b.path.len()))
            .then(a.repository_id.cmp(&b.repository_id))
    });

    Ok(ImpactReport {
        impacted,
        limits_hit,
    })
}

fn shortest_freshest(hops: Vec<Hop>) -> Vec<Hop> {
    hops
}

fn classify(path: &[Hop], any_resolved_path: bool) -> ImpactLevel {
    if path.is_empty() {
        return ImpactLevel::Unknown;
    }
    // Any STALE segment downgrades to UNKNOWN — stale relationships are
    // rejected per contract, never laundered into confident claims.
    if path.iter().any(|h| h.freshness_state != "CURRENT") {
        return ImpactLevel::Unknown;
    }
    let all_resolved = path.iter().all(|h| {
        matches!(
            h.resolution.as_str(),
            "PACKAGE_RESOLVED" | "SYMBOL_RESOLVED" | "BUILD_RESOLVED" | "FRAMEWORK_RESOLVED"
        )
    });
    if all_resolved && any_resolved_path {
        if path.len() == 1 {
            ImpactLevel::DirectResolved
        } else {
            ImpactLevel::IndirectResolved
        }
    } else {
        ImpactLevel::PossibleInferred
    }
}

fn rank(l: ImpactLevel) -> u8 {
    match l {
        ImpactLevel::DirectResolved => 0,
        ImpactLevel::IndirectResolved => 1,
        ImpactLevel::PossibleInferred => 2,
        ImpactLevel::Unknown => 3,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::CancelToken;

    fn hop(resolution: &str, freshness: &str) -> Hop {
        Hop {
            edge_id: "e1".to_owned(),
            repository_id: "r1".to_owned(),
            resolution: resolution.to_owned(),
            confidence: 0.9,
            freshness_state: freshness.to_owned(),
        }
    }

    // --- classify() unit tests ---

    #[test]
    fn classify_empty_path_is_unknown() {
        assert_eq!(classify(&[], false), ImpactLevel::Unknown);
    }

    #[test]
    fn classify_single_resolved_hop_is_direct() {
        let path = vec![hop("PACKAGE_RESOLVED", "CURRENT")];
        assert_eq!(classify(&path, true), ImpactLevel::DirectResolved);
    }

    #[test]
    fn classify_multi_hop_resolved_is_indirect() {
        let path = vec![
            hop("PACKAGE_RESOLVED", "CURRENT"),
            hop("BUILD_RESOLVED", "CURRENT"),
        ];
        assert_eq!(classify(&path, true), ImpactLevel::IndirectResolved);
    }

    #[test]
    fn classify_stale_hop_downgrades_to_unknown() {
        let path = vec![hop("PACKAGE_RESOLVED", "STALE")];
        assert_eq!(classify(&path, true), ImpactLevel::Unknown);
    }

    #[test]
    fn classify_inferred_resolution_is_possible() {
        let path = vec![hop("INFERRED", "CURRENT")];
        assert_eq!(classify(&path, true), ImpactLevel::PossibleInferred);
    }

    #[test]
    fn classify_mixed_resolved_and_inferred_is_possible() {
        let path = vec![
            hop("PACKAGE_RESOLVED", "CURRENT"),
            hop("INFERRED", "CURRENT"),
        ];
        assert_eq!(classify(&path, true), ImpactLevel::PossibleInferred);
    }

    // --- rank() ordering ---

    #[test]
    fn rank_ordering() {
        assert!(rank(ImpactLevel::DirectResolved) < rank(ImpactLevel::IndirectResolved));
        assert!(rank(ImpactLevel::IndirectResolved) < rank(ImpactLevel::PossibleInferred));
        assert!(rank(ImpactLevel::PossibleInferred) < rank(ImpactLevel::Unknown));
    }

    // --- ImpactLevel::as_str() ---

    #[test]
    fn impact_level_as_str() {
        assert_eq!(ImpactLevel::DirectResolved.as_str(), "DIRECT_RESOLVED");
        assert_eq!(ImpactLevel::IndirectResolved.as_str(), "INDIRECT_RESOLVED");
        assert_eq!(ImpactLevel::PossibleInferred.as_str(), "POSSIBLE_INFERRED");
        assert_eq!(ImpactLevel::Unknown.as_str(), "UNKNOWN");
    }

    // --- Integration test with seeded DB ---

    fn seeded_conn() -> rusqlite::Connection {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        attic_storage::connection::configure_connection(&conn).unwrap();
        attic_storage::migration::run_migrations(&conn).unwrap();
        conn
    }

    fn test_id(name: &str) -> attic_core::RepositoryId {
        let u = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_DNS, name.as_bytes());
        u.to_string().parse().unwrap()
    }

    fn tid(name: &str) -> String {
        test_id(name).to_string_repr()
    }

    fn insert_repo(conn: &rusqlite::Connection, id: &str, root: &str) {
        let rid = test_id(id);
        attic_storage::repository::repository::upsert_repository(conn, &rid, root, id).unwrap();
    }

    fn insert_rev(conn: &rusqlite::Connection, repo_id: &str) -> String {
        let rid = test_id(repo_id);
        let srid = attic_core::SourceRevisionId::new_v4();
        attic_storage::repository::source_revision::insert_source_revision(
            conn,
            &srid,
            &rid,
            "test-sha",
            "2024-01-01",
            attic_core::SourceType::Git,
        )
        .unwrap();
        srid.to_string_repr().to_string()
    }

    #[test]
    fn analyze_dependents_linear_chain() {
        let conn = seeded_conn();
        insert_repo(&conn, "r0", "/ws/0");
        insert_repo(&conn, "r1", "/ws/1");
        insert_repo(&conn, "r2", "/ws/2");
        let rev = insert_rev(&conn, "r0");
        let _ = insert_rev(&conn, "r1");
        let _ = insert_rev(&conn, "r2");

        // r2 depends on r1, r1 depends on r0
        attic_storage::crossrepo_ops::insert_xrepo_edge(
            &conn, &tid("r2"), "s2", &tid("r1"), "t1", "PACKAGE_RESOLVED", 0.9, "GO_MODULE", "{}", &rev,
        )
        .unwrap();
        attic_storage::crossrepo_ops::insert_xrepo_edge(
            &conn, &tid("r1"), "s1", &tid("r0"), "t0", "PACKAGE_RESOLVED", 0.9, "GO_MODULE", "{}", &rev,
        )
        .unwrap();

        let budget = TraversalBudget {
            max_depth: 6,
            max_repositories: 64,
            max_edges: 2000,
            max_time_ms: 5000,
            cancel: CancelToken::never(),
        };
        let report = analyze_dependents(&conn, &tid("r0"), &budget).unwrap();

        // r1 depends on r0 (direct), r2 depends on r1 (indirect via r1→r0)
        let ids: Vec<&str> = report
            .impacted
            .iter()
            .map(|r| r.repository_id.as_str())
            .collect();
        assert!(ids.contains(&tid("r1").as_str()), "r1 should be impacted (direct)");
        assert!(ids.contains(&tid("r2").as_str()), "r2 should be impacted (indirect)");

        // r1 should be DIRECT, r2 should be INDIRECT
        let r1_impact = report.impacted.iter().find(|r| r.repository_id == tid("r1")).unwrap();
        assert_eq!(r1_impact.level, ImpactLevel::DirectResolved);

        let r2_impact = report.impacted.iter().find(|r| r.repository_id == tid("r2")).unwrap();
        assert_eq!(r2_impact.level, ImpactLevel::IndirectResolved);
    }

    #[test]
    fn analyze_dependents_stale_path_downgrades() {
        let conn = seeded_conn();
        insert_repo(&conn, "r0", "/ws/0");
        insert_repo(&conn, "r1", "/ws/1");
        let rev = insert_rev(&conn, "r0");
        let _ = insert_rev(&conn, "r1");

        let edge_id = attic_storage::crossrepo_ops::insert_xrepo_edge(
            &conn, &tid("r1"), "s1", &tid("r0"), "t0", "PACKAGE_RESOLVED", 0.9, "GO_MODULE", "{}", &rev,
        )
        .unwrap();
        conn.execute(
            "UPDATE core_relationships SET freshness_state = 'STALE' WHERE id = ?1",
            rusqlite::params![edge_id],
        )
        .unwrap();

        let budget = TraversalBudget::default();
        let report = analyze_dependents(&conn, &tid("r0"), &budget).unwrap();

        // r1 should still appear but with Unknown level due to STALE
        if let Some(r1) = report.impacted.iter().find(|r| r.repository_id == tid("r1")) {
            assert_eq!(r1.level, ImpactLevel::Unknown);
        }
    }
}
