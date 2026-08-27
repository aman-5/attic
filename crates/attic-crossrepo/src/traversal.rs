//! Budgeted workspace graph traversal (Phase 6 §8).
//!
//! Traversal is EVIDENCE EXPANSION, never truth.  Every walk enforces:
//! max depth, max nodes (repositories visited), max edges, max time and
//! cooperative cancellation — plus cycle safety (`A → B → C → A` cannot
//! explode retrieval because each repository is expanded at most once).
//!
//! Only cross-repository `DEPENDS_ON` edges participate here; repository
//! isolation is preserved because traversal never re-opens repository-local
//! intelligence.

use std::collections::{HashSet, VecDeque};
use std::time::Instant;

use rusqlite::Connection;

use crate::{CancelToken, limits};
use attic_storage::XrepoEdge;

/// Hard bounds for one workspace traversal.
#[derive(Debug, Clone)]
pub struct TraversalBudget {
    /// Maximum hops from the seed.
    pub max_depth: u32,
    /// Maximum repositories visited.
    pub max_repositories: u32,
    /// Maximum edges examined.
    pub max_edges: u32,
    /// Wall-clock ceiling in milliseconds.
    pub max_time_ms: u64,
    /// Cooperative cancellation.
    pub cancel: CancelToken,
}

impl Default for TraversalBudget {
    fn default() -> Self {
        Self {
            max_depth: 6,
            max_repositories: 64,
            max_edges: 2_000,
            max_time_ms: 5_000,
            cancel: CancelToken::never(),
        }
    }
}

/// Direction of a workspace walk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Follow `source → target`: what does this repository depend on.
    Dependencies,
    /// Follow `target → source`: what depends on this repository.
    Dependents,
}

/// Observable outcome of one bounded traversal.
#[derive(Debug, Default, Clone)]
pub struct TraversalOutcome {
    /// Edges traversed in deterministic BFS order.
    pub edges: Vec<XrepoEdge>,
    /// Repositories reached (excluding the seed), in visit order.
    pub repositories: Vec<String>,
    /// Deepest hop actually used.
    pub depth_reached: u32,
    /// Which budgets were exhausted (`depth|repositories|edges|time`), in
    /// first-hit order.  Empty ⇒ completed within budget.
    pub limits_hit: Vec<&'static str>,
    /// True when cancellation was observed.
    pub cancelled: bool,
}

/// Walk the workspace graph from `seed_repo` in `direction`.
///
/// INVALID edges are excluded by the storage layer; STALE edges are
/// returned carrying their freshness so callers classify per contract.
pub fn traverse(
    conn: &Connection,
    seed_repo: &str,
    direction: Direction,
    budget: &TraversalBudget,
) -> Result<TraversalOutcome, crate::error::CrossRepoError> {
    let started = Instant::now();
    let mut out = TraversalOutcome::default();

    let mut visited_repos: HashSet<String> = HashSet::new();
    let mut seen_edges: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<(String, u32)> = VecDeque::new();
    queue.push_back((seed_repo.to_string(), 0));
    visited_repos.insert(seed_repo.to_string());

    'outer: while let Some((repo, depth)) = queue.pop_front() {
        if budget.cancel.is_cancelled() {
            out.cancelled = true;
            break;
        }
        if depth >= budget.max_depth {
            out.limits_hit.push("depth");
            continue;
        }
        if out.repositories.len() as u32 >= budget.max_repositories {
            out.limits_hit.push("repositories");
            break;
        }
        if started.elapsed().as_millis() as u64 >= budget.max_time_ms {
            out.limits_hit.push("time");
            break;
        }

        // Per-repository edge fetch is indexed (cross-repo partial index);
        // the per-call cap keeps any single hub from dominating a batch.
        let mut edges =
            attic_storage::cross_edges_touching(conn, &repo, limits::MAX_CANDIDATES_PER_KEY)?;
        edges.sort_by(|a, b| a.id.cmp(&b.id));

        for e in edges {
            if seen_edges.contains(&e.id) {
                continue;
            }
            if e.freshness_state == "INVALID" {
                continue;
            }
            seen_edges.insert(e.id.clone());

            let forward_next = match direction {
                Direction::Dependencies => {
                    if e.source_repository_id == repo {
                        Some(e.target_repository_id.clone())
                    } else {
                        None
                    }
                }
                Direction::Dependents => {
                    if e.target_repository_id == repo {
                        Some(e.source_repository_id.clone())
                    } else {
                        None
                    }
                }
            };
            let Some(next) = forward_next else {
                continue; // wrong direction for this edge
            };

            out.edges.push(e);
            if out.edges.len() as u32 >= budget.max_edges {
                out.limits_hit.push("edges");
                break 'outer;
            }

            if !visited_repos.contains(&next) {
                visited_repos.insert(next.clone());
                out.repositories.push(next.clone());
                out.depth_reached = out.depth_reached.max(depth + 1);
                queue.push_back((next, depth + 1));
                if out.repositories.len() as u32 >= budget.max_repositories {
                    out.limits_hit.push("repositories");
                    break 'outer;
                }
            }

            if started.elapsed().as_millis() as u64 >= budget.max_time_ms {
                out.limits_hit.push("time");
                break 'outer;
            }
        }
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use attic_storage::connection::configure_connection;
    use attic_storage::migration::run_migrations;

    fn seeded_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        configure_connection(&conn).unwrap();
        run_migrations(&conn).unwrap();
        conn
    }

    fn test_id(name: &str) -> attic_core::RepositoryId {
        let u = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_DNS, name.as_bytes());
        u.to_string().parse().unwrap()
    }

    fn insert_repo(conn: &Connection, id: &str, root: &str) {
        let rid = test_id(id);
        attic_storage::repository::repository::upsert_repository(conn, &rid, root, id).unwrap();
    }

    fn insert_rev(conn: &Connection, repo_id: &str) -> String {
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

    fn insert_dep_edge(
        conn: &Connection,
        src_repo: &str,
        tgt_repo: &str,
        src_entity: &str,
        tgt_entity: &str,
        rev: &str,
    ) {
        let src_id = test_id(src_repo).to_string_repr();
        let tgt_id = test_id(tgt_repo).to_string_repr();
        attic_storage::crossrepo_ops::insert_xrepo_edge(
            conn,
            &src_id,
            src_entity,
            &tgt_id,
            tgt_entity,
            "PACKAGE_RESOLVED",
            0.9,
            "GO_MODULE",
            "{}",
            rev,
        )
        .unwrap();
    }

    fn tid(name: &str) -> String {
        test_id(name).to_string_repr()
    }

    #[test]
    fn single_seed_no_edges() {
        let conn = seeded_conn();
        insert_repo(&conn, "repo-a", "/ws/a");
        let budget = TraversalBudget {
            max_depth: 6,
            max_repositories: 64,
            max_edges: 2000,
            max_time_ms: 5000,
            cancel: CancelToken::never(),
        };
        let outcome = traverse(&conn, &tid("repo-a"), Direction::Dependencies, &budget).unwrap();
        assert!(outcome.edges.is_empty());
        assert!(outcome.repositories.is_empty());
        assert!(outcome.limits_hit.is_empty());
    }

    #[test]
    fn two_repo_linear_chain() {
        let conn = seeded_conn();
        insert_repo(&conn, "repo-a", "/ws/a");
        insert_repo(&conn, "repo-b", "/ws/b");
        let rev_a = insert_rev(&conn, "repo-a");

        // A → B (A depends on B)
        insert_dep_edge(&conn, "repo-a", "repo-b", "src-a", "tgt-b", &rev_a);

        let budget = TraversalBudget {
            max_depth: 6,
            max_repositories: 64,
            max_edges: 2000,
            max_time_ms: 5000,
            cancel: CancelToken::never(),
        };

        // Forward: A depends on what? Should reach B.
        let out = traverse(&conn, &tid("repo-a"), Direction::Dependencies, &budget).unwrap();
        assert_eq!(out.repositories, vec![tid("repo-b")]);
        assert_eq!(out.edges.len(), 1);
        assert_eq!(out.edges[0].target_repository_id, tid("repo-b"));
        assert_eq!(out.depth_reached, 1);
    }

    #[test]
    fn three_repo_chain_depth_enforced() {
        let conn = seeded_conn();
        insert_repo(&conn, "r1", "/ws/1");
        insert_repo(&conn, "r2", "/ws/2");
        insert_repo(&conn, "r3", "/ws/3");
        let rev = insert_rev(&conn, "r1");
        let _ = insert_rev(&conn, "r2");
        let _ = insert_rev(&conn, "r3");

        insert_dep_edge(&conn, "r1", "r2", "s1", "t2", &rev);
        insert_dep_edge(&conn, "r2", "r3", "s2", "t3", &rev);

        // max_depth=1: should only reach r2, not r3
        let budget = TraversalBudget {
            max_depth: 1,
            max_repositories: 64,
            max_edges: 2000,
            max_time_ms: 5000,
            cancel: CancelToken::never(),
        };
        let out = traverse(&conn, &tid("r1"), Direction::Dependencies, &budget).unwrap();
        // r1 → r2 only (depth 1)
        assert!(out.repositories.contains(&tid("r2")));
        assert!(
            !out.repositories.contains(&tid("r3")),
            "r3 should not be reached at depth 1"
        );
    }

    #[test]
    fn max_repositories_limits_traversal() {
        let conn = seeded_conn();
        // Create 5 repos in a chain: r0 → r1 → r2 → r3 → r4
        for i in 0..5 {
            insert_repo(&conn, &format!("r{i}"), &format!("/ws/{i}"));
        }
        let rev = insert_rev(&conn, "r0");
        for i in 0..4 {
            insert_dep_edge(
                &conn,
                &format!("r{i}"),
                &format!("r{}", i + 1),
                &format!("s{i}"),
                &format!("t{}", i + 1),
                &rev,
            );
        }

        let budget = TraversalBudget {
            max_depth: 10,
            max_repositories: 2, // only 2 new repos allowed
            max_edges: 2000,
            max_time_ms: 5000,
            cancel: CancelToken::never(),
        };
        let out = traverse(&conn, &tid("r0"), Direction::Dependencies, &budget).unwrap();
        assert!(
            out.repositories.len() <= 2,
            "should visit at most 2 repos, got {}",
            out.repositories.len()
        );
        assert!(out.limits_hit.contains(&"repositories"));
    }

    #[test]
    fn cycle_does_not_explode() {
        let conn = seeded_conn();
        insert_repo(&conn, "r0", "/ws/0");
        insert_repo(&conn, "r1", "/ws/1");
        let rev = insert_rev(&conn, "r0");
        let _ = insert_rev(&conn, "r1");

        // r0 → r1 → r0 (cycle)
        insert_dep_edge(&conn, "r0", "r1", "s0", "t1", &rev);
        insert_dep_edge(&conn, "r1", "r0", "s1", "t0", &rev);

        let budget = TraversalBudget {
            max_depth: 10,
            max_repositories: 64,
            max_edges: 2000,
            max_time_ms: 5000,
            cancel: CancelToken::never(),
        };
        let out = traverse(&conn, &tid("r0"), Direction::Dependencies, &budget).unwrap();
        // Cycle should terminate — each repo visited once
        assert_eq!(out.repositories, vec![tid("r1")]);
        assert!(!out.cancelled);
    }

    #[test]
    fn cancel_token_stops_traversal() {
        let conn = seeded_conn();
        insert_repo(&conn, "r0", "/ws/0");
        insert_repo(&conn, "r1", "/ws/1");
        let rev = insert_rev(&conn, "r0");
        insert_dep_edge(&conn, "r0", "r1", "s0", "t1", &rev);

        let cancel = CancelToken::never();
        cancel.cancel(); // pre-cancel

        let budget = TraversalBudget {
            max_depth: 10,
            max_repositories: 64,
            max_edges: 2000,
            max_time_ms: 5000,
            cancel,
        };
        let out = traverse(&conn, &tid("r0"), Direction::Dependencies, &budget).unwrap();
        assert!(out.cancelled);
        assert!(out.repositories.is_empty());
    }

    #[test]
    fn dependents_direction_reverse() {
        let conn = seeded_conn();
        insert_repo(&conn, "r0", "/ws/0");
        insert_repo(&conn, "r1", "/ws/1");
        insert_repo(&conn, "r2", "/ws/2");
        let rev = insert_rev(&conn, "r0");
        let _ = insert_rev(&conn, "r1");
        let _ = insert_rev(&conn, "r2");

        // r0 → r1 → r2 (r0 depends on r1, r1 depends on r2)
        insert_dep_edge(&conn, "r0", "r1", "s0", "t1", &rev);
        insert_dep_edge(&conn, "r1", "r2", "s1", "t2", &rev);

        // From r2, Dependents direction: who depends on r2? → r1 → r0
        let budget = TraversalBudget {
            max_depth: 10,
            max_repositories: 64,
            max_edges: 2000,
            max_time_ms: 5000,
            cancel: CancelToken::never(),
        };
        let out = traverse(&conn, &tid("r2"), Direction::Dependents, &budget).unwrap();
        assert!(out.repositories.contains(&tid("r1")));
        assert!(out.repositories.contains(&tid("r0")));
    }

    #[test]
    fn stale_edges_are_traversed() {
        let conn = seeded_conn();
        insert_repo(&conn, "r0", "/ws/0");
        insert_repo(&conn, "r1", "/ws/1");
        let rev = insert_rev(&conn, "r0");

        // Insert edge then mark it STALE
        let edge_id = attic_storage::crossrepo_ops::insert_xrepo_edge(
            &conn,
            &tid("r0"),
            "s0",
            &tid("r1"),
            "t1",
            "PACKAGE_RESOLVED",
            0.9,
            "GO_MODULE",
            "{}",
            &rev,
        )
        .unwrap();
        conn.execute(
            "UPDATE core_relationships SET freshness_state = 'STALE' WHERE id = ?1",
            rusqlite::params![edge_id],
        )
        .unwrap();

        let budget = TraversalBudget {
            max_depth: 6,
            max_repositories: 64,
            max_edges: 2000,
            max_time_ms: 5000,
            cancel: CancelToken::never(),
        };
        let out = traverse(&conn, &tid("r0"), Direction::Dependencies, &budget).unwrap();
        // STALE edges are included (INVALID are excluded)
        assert_eq!(out.repositories, vec![tid("r1")]);
    }

    #[test]
    fn invalid_edges_are_excluded() {
        let conn = seeded_conn();
        insert_repo(&conn, "r0", "/ws/0");
        insert_repo(&conn, "r1", "/ws/1");
        let rev = insert_rev(&conn, "r0");

        let edge_id = attic_storage::crossrepo_ops::insert_xrepo_edge(
            &conn,
            &tid("r0"),
            "s0",
            &tid("r1"),
            "t1",
            "PACKAGE_RESOLVED",
            0.9,
            "GO_MODULE",
            "{}",
            &rev,
        )
        .unwrap();
        conn.execute(
            "UPDATE core_relationships SET freshness_state = 'INVALID' WHERE id = ?1",
            rusqlite::params![edge_id],
        )
        .unwrap();

        let budget = TraversalBudget {
            max_depth: 6,
            max_repositories: 64,
            max_edges: 2000,
            max_time_ms: 5000,
            cancel: CancelToken::never(),
        };
        let out = traverse(&conn, &tid("r0"), Direction::Dependencies, &budget).unwrap();
        assert!(
            out.repositories.is_empty(),
            "INVALID edges must be excluded"
        );
    }
}
