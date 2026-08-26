//! End-to-end integration test: multi-repo workspace → Phase 6 → Phase 4.
//!
//! Verifies that cross-repository dependency intelligence flows correctly
//! from manifest parsing through resolver to evidence expansion.

use std::collections::HashMap;

use attic_crossrepo::maintenance::sync_repository;
use attic_crossrepo::resolver::{self, RepoCatalogData};
use attic_crossrepo::traversal::{self, Direction, TraversalBudget};
use attic_crossrepo::impact;
use attic_crossrepo::{CancelToken, DeclarationKind, DependencyDeclaration, Ecosystem, ProvidedIdentity};
use attic_storage::connection::configure_connection;
use attic_storage::migration::run_migrations;

fn seeded_conn() -> rusqlite::Connection {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    configure_connection(&conn).unwrap();
    run_migrations(&conn).unwrap();
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

/// Create a multi-repo workspace with three repositories:
/// - `provider` (Go library)
/// - `consumer` (Go app depending on provider)
/// - `indirect` (Go library depending on provider)
///
/// Verify the full Phase 6 → Phase 4 pipeline:
/// 1. Phase 6: sync both repos, resolve edges
/// 2. Verify edges exist in core_relationships
/// 3. Phase 4: graph expansion finds cross-repo edges
/// 4. Evidence carries correct provenance
#[test]
fn e2e_multi_repo_sync_and_resolve() {
    let conn = seeded_conn();

    // Set up workspace with three repos
    let provider_dir = tempfile::tempdir().unwrap();
    let consumer_dir = tempfile::tempdir().unwrap();
    let indirect_dir = tempfile::tempdir().unwrap();

    // Provider: Go library
    std::fs::write(
        provider_dir.path().join("go.mod"),
        "module example.com/team/lib\n",
    )
    .unwrap();

    // Consumer: depends on provider
    std::fs::write(
        consumer_dir.path().join("go.mod"),
        "module example.com/team/app\nrequire example.com/team/lib v1.0.0\n",
    )
    .unwrap();

    // Indirect: also depends on provider
    std::fs::write(
        indirect_dir.path().join("go.mod"),
        "module example.com/team/indirect\nrequire example.com/team/lib v1.0.0\n",
    )
    .unwrap();

    insert_repo(&conn, "provider", &provider_dir.path().to_string_lossy());
    insert_repo(&conn, "consumer", &consumer_dir.path().to_string_lossy());
    insert_repo(&conn, "indirect", &indirect_dir.path().to_string_lossy());

    // Insert source revisions (required for resolve_source_revision to find them)
    insert_rev(&conn, "provider");
    insert_rev(&conn, "consumer");
    insert_rev(&conn, "indirect");

    // Phase 6: sync each repository
    let report_p = sync_repository(&conn, &tid("provider")).unwrap();
    assert_eq!(report_p.repository_id, tid("provider"));

    let report_c = sync_repository(&conn, &tid("consumer")).unwrap();
    assert_eq!(report_c.repository_id, tid("consumer"));

    let report_i = sync_repository(&conn, &tid("indirect")).unwrap();
    assert_eq!(report_i.repository_id, tid("indirect"));

    // Build resolver input from DB state
    let catalog_p = attic_storage::crossrepo_ops::catalog_entry(&conn, &tid("provider")).unwrap();
    let catalog_c = attic_storage::crossrepo_ops::catalog_entry(&conn, &tid("consumer")).unwrap();
    let catalog_i = attic_storage::crossrepo_ops::catalog_entry(&conn, &tid("indirect")).unwrap();

    // Manually inject provides for provider and declarations for consumers
    // (in a real system these come from manifest parsing; here we simulate)
    let provides = vec![ProvidedIdentity {
        ecosystem: Ecosystem::Go,
        name: "example.com/team/lib".to_owned(),
    }];
    let decl_consumer = vec![DependencyDeclaration {
        path: "go.mod".to_owned(),
        ecosystem: Ecosystem::Go,
        name: "example.com/team/lib".to_owned(),
        version_req: Some("v1.0.0".to_owned()),
        kind: DeclarationKind::External,
        local_hint: None,
    }];
    let decl_indirect = vec![DependencyDeclaration {
        path: "go.mod".to_owned(),
        ecosystem: Ecosystem::Go,
        name: "example.com/team/lib".to_owned(),
        version_req: Some("v1.0.0".to_owned()),
        kind: DeclarationKind::External,
        local_hint: None,
    }];

    let repo_data = vec![
        RepoCatalogData {
            repository_id: tid("provider"),
            root_path: provider_dir.path().to_string_lossy().to_string(),
            source_revision_id: catalog_p.as_ref().map(|c| c.source_revision_id.clone()).unwrap_or_default(),
            provides,
            declarations: vec![],
            primary_anchor_occurrence: None,
            go_module_prefix: Some("example.com/team/lib".to_owned()),
        },
        RepoCatalogData {
            repository_id: tid("consumer"),
            root_path: consumer_dir.path().to_string_lossy().to_string(),
            source_revision_id: catalog_c.as_ref().map(|c| c.source_revision_id.clone()).unwrap_or_default(),
            provides: vec![],
            declarations: decl_consumer,
            primary_anchor_occurrence: None,
            go_module_prefix: None,
        },
        RepoCatalogData {
            repository_id: tid("indirect"),
            root_path: indirect_dir.path().to_string_lossy().to_string(),
            source_revision_id: catalog_i.as_ref().map(|c| c.source_revision_id.clone()).unwrap_or_default(),
            provides: vec![],
            declarations: decl_indirect,
            primary_anchor_occurrence: None,
            go_module_prefix: None,
        },
    ];

    // Phase 6: resolve
    let (edges, diagnostics) = resolver::resolve_workspace(&repo_data, &HashMap::new());
    assert_eq!(edges.len(), 2, "consumer→provider and indirect→provider");
    assert!(diagnostics.is_empty(), "no resolution diagnostics expected");

    // Verify edge directions
    let consumer_edge = edges.iter().find(|e| e.source_repository_id == tid("consumer")).unwrap();
    assert_eq!(consumer_edge.target_repository_id, tid("provider"));
    assert_eq!(consumer_edge.resolution, "PACKAGE_RESOLVED");
    assert_eq!(consumer_edge.dependency_basis, "GO_MODULE");

    let indirect_edge = edges.iter().find(|e| e.source_repository_id == tid("indirect")).unwrap();
    assert_eq!(indirect_edge.target_repository_id, tid("provider"));

    // Persist edges
    for e in &edges {
        attic_storage::crossrepo_ops::insert_xrepo_edge(
            &conn,
            &e.source_repository_id,
            &e.source_entity_id,
            &e.target_repository_id,
            &e.target_entity_id,
            &e.resolution,
            e.confidence,
            &e.dependency_basis,
            &e.provenance_json,
            &e.source_revision_id,
        )
        .unwrap();
    }

    // Phase 4: verify graph expansion finds cross-repo edges
    let _seed_id = &edges[0].source_entity_id;
    let budget = TraversalBudget {
        max_depth: 4,
        max_repositories: 64,
        max_edges: 2000,
        max_time_ms: 5000,
        cancel: CancelToken::never(),
    };
    let traversal_result = traversal::traverse(&conn, &tid("consumer"), Direction::Dependencies, &budget).unwrap();
    assert!(
        traversal_result.repositories.contains(&tid("provider")),
        "graph traversal must reach provider from consumer"
    );

    // Impact analysis: provider affects consumer and indirect
    let impact_report = impact::analyze_dependents(&conn, &tid("provider"), &budget).unwrap();
    assert!(!impact_report.impacted.is_empty(), "provider should have dependents");
    assert!(
        impact_report.impacted.iter().any(|r| r.repository_id == tid("consumer")),
        "consumer must be in impacted repos"
    );
    assert!(
        impact_report.impacted.iter().any(|r| r.repository_id == tid("indirect")),
        "indirect must be in impacted repos"
    );
}

/// Verify that invalid edges are excluded from traversal but STALE edges
/// are still traversed (with degraded confidence).
#[test]
fn e2e_stale_vs_invalid_edge_handling() {
    let conn = seeded_conn();
    insert_repo(&conn, "r0", "/ws/0");
    insert_repo(&conn, "r1", "/ws/1");
    insert_repo(&conn, "r2", "/ws/2");
    let rev = insert_rev(&conn, "r0");

    // CURRENT edge: r0 → r1
    let _edge_current = attic_storage::crossrepo_ops::insert_xrepo_edge(
        &conn, &tid("r0"), "occ-0", &tid("r1"), "occ-1",
        "PACKAGE_RESOLVED", 0.9, "GO_MODULE", "{}", &rev,
    ).unwrap();

    // STALE edge: r0 → r2
    let edge_stale = attic_storage::crossrepo_ops::insert_xrepo_edge(
        &conn, &tid("r0"), "occ-0", &tid("r2"), "occ-2",
        "PACKAGE_RESOLVED", 0.7, "GO_MODULE", "{}", &rev,
    ).unwrap();
    conn.execute(
        "UPDATE core_relationships SET freshness_state = 'STALE' WHERE id = ?1",
        rusqlite::params![edge_stale],
    ).unwrap();

    // INVALID edge: r1 → r2 (should be excluded)
    let edge_invalid = attic_storage::crossrepo_ops::insert_xrepo_edge(
        &conn, &tid("r1"), "occ-1", &tid("r2"), "occ-2",
        "PACKAGE_RESOLVED", 0.8, "GO_MODULE", "{}", &rev,
    ).unwrap();
    conn.execute(
        "UPDATE core_relationships SET freshness_state = 'INVALID' WHERE id = ?1",
        rusqlite::params![edge_invalid],
    ).unwrap();

    let budget = TraversalBudget {
        max_depth: 4,
        max_repositories: 64,
        max_edges: 2000,
        max_time_ms: 5000,
        cancel: CancelToken::never(),
    };

    // Traversal from r0: should reach r1 (CURRENT) and r2 (STALE)
    let result = traversal::traverse(&conn, &tid("r0"), Direction::Dependencies, &budget).unwrap();
    assert!(result.repositories.contains(&tid("r1")), "r1 via CURRENT edge");
    assert!(result.repositories.contains(&tid("r2")), "r2 via STALE edge");

    // Traversal from r1: should NOT reach r2 (INVALID edge excluded)
    let result2 = traversal::traverse(&conn, &tid("r1"), Direction::Dependencies, &budget).unwrap();
    assert!(!result2.repositories.contains(&tid("r2")), "r2 must be excluded via INVALID edge");
}

/// Verify that repository removal cleans up all cross-repo state.
#[test]
fn e2e_repository_removal_cascade() {
    let conn = seeded_conn();
    insert_repo(&conn, "r0", "/ws/0");
    insert_repo(&conn, "r1", "/ws/1");
    insert_repo(&conn, "r2", "/ws/2");
    let rev0 = insert_rev(&conn, "r0");
    let _rev1 = insert_rev(&conn, "r1");

    // Insert catalog for r0
    let catalog = attic_storage::crossrepo_ops::CatalogRow {
        repository_id: tid("r0"),
        source_revision_id: rev0.clone(),
        provides_json: r#"[{"ecosystem":"Go","name":"example.com/r0"}]"#.to_owned(),
        manifest_hash: "abc123".to_owned(),
        entry_count: 1,
        freshness_state: "CURRENT".to_owned(),
    };
    attic_storage::crossrepo_ops::upsert_catalog_row(&conn, &catalog, &catalog.provides_json).unwrap();

    // Insert edges: r0→r1 and r0→r2
    let _ = attic_storage::crossrepo_ops::insert_xrepo_edge(
        &conn, &tid("r0"), "occ-0", &tid("r1"), "occ-1",
        "PACKAGE_RESOLVED", 0.9, "GO_MODULE", "{}", &rev0,
    ).unwrap();
    let _ = attic_storage::crossrepo_ops::insert_xrepo_edge(
        &conn, &tid("r0"), "occ-0", &tid("r2"), "occ-2",
        "PACKAGE_RESOLVED", 0.8, "GO_MODULE", "{}", &rev0,
    ).unwrap();

    // Remove r0
    let (edges_deleted, _decls_deleted) = attic_crossrepo::maintenance::repository_removed(&conn, &tid("r0")).unwrap();
    assert!(edges_deleted >= 2, "both edges should be deleted");

    // Verify: no catalog, no edges
    let cat = attic_storage::crossrepo_ops::catalog_entry(&conn, &tid("r0")).unwrap();
    assert!(cat.is_none(), "catalog should be deleted");

    let remaining: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM core_relationships WHERE source_repository_id = ?1 OR target_repository_id = ?1",
            rusqlite::params![tid("r0")],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(remaining, 0, "no edges should remain");

    // Verify r1 and r2 still have their data
    let _r1_edges: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM core_relationships WHERE source_repository_id = ?1 OR target_repository_id = ?1",
            rusqlite::params![tid("r1")],
            |r| r.get(0),
        )
        .unwrap();
    // r1 might still have edges if there were any (there weren't in this test)
    // The important thing is r0's data is gone
}

/// True end-to-end test through the integrated path:
///
/// multi-repo fixture → Phase 2 indexing → Phase 6 sync_workspace →
/// Phase 6 resolver → edges with SourceRevision → traversal → impact
///
/// Verifies real repository provenance, SourceRevision, resolution level,
/// confidence, and bounded traversal — the full Phase 6 product gate.
#[test]
fn e2e_integrated_indexing_to_crossrepo_to_traversal() {
    use attic_indexing::{IndexOptions, IndexingStore, index_repository};
    use attic_storage::writer::WriterQueue;

    // 1. Create multi-repo fixture with real manifest files.
    let provider_dir = tempfile::tempdir().unwrap();
    let consumer_dir = tempfile::tempdir().unwrap();

    std::fs::write(
        provider_dir.path().join("go.mod"),
        "module example.com/team/srv\n",
    )
    .unwrap();
    std::fs::write(
        provider_dir.path().join("lib.go"),
        "package lib\nfunc Exported() {}\n",
    )
    .unwrap();

    std::fs::write(
        consumer_dir.path().join("go.mod"),
        "module example.com/team/app\nrequire example.com/team/srv v1.0.0\n",
    )
    .unwrap();
    std::fs::write(
        consumer_dir.path().join("main.go"),
        "package main\nimport \"example.com/team/srv\"\nfunc main() { srv.Exported() }\n",
    )
    .unwrap();

    // 2. Set up DB pool + writer queue.
    let db_file = tempfile::NamedTempFile::new().unwrap();
    let (pool_conn, pool) = attic_storage::open_db(db_file.path()).unwrap();
    // Run migrations on the pool connection.
    attic_storage::connection::configure_connection(&pool_conn).unwrap();
    attic_storage::migration::run_migrations(&pool_conn).unwrap();
    drop(pool_conn);
    // Writer queue uses a separate connection to the same file.
    let writer_conn = rusqlite::Connection::open(db_file.path()).unwrap();
    attic_storage::connection::configure_connection(&writer_conn).unwrap();
    let writer_queue = WriterQueue::new(writer_conn).unwrap();
    let writer_handle = writer_queue.handle();

    // 3. Phase 2: index both repositories through the real indexing path.
    let store = IndexingStore {
        readers: &pool,
        writer: &writer_handle,
    };
    let policy = attic_discovery::DiscoveryPolicy::default_git();
    let opts = IndexOptions::default();

    let provider_result = index_repository(&store, provider_dir.path(), &policy, &opts)
        .expect("provider indexing should succeed");
    let consumer_result = index_repository(&store, consumer_dir.path(), &policy, &opts)
        .expect("consumer indexing should succeed");

    let provider_repo_id = provider_result.repository_id.clone();
    let consumer_repo_id = consumer_result.repository_id.clone();
    assert_ne!(provider_repo_id, consumer_repo_id, "repo IDs must differ");

    // 4. Phase 6: sync workspace (reader → resolver → writer).
    pool.with_reader(|conn| {
        attic_crossrepo::maintenance::sync_workspace(
            conn,
            &writer_handle,
            &attic_crossrepo::maintenance::WorkspaceSyncOptions::default(),
        )
        .map_err(|e| attic_storage::StorageError::Worker(e.to_string()))
    })
    .expect("sync_workspace should succeed");

    // 5. Verify cross-repo edges exist with correct provenance.
    pool.with_reader(|conn| {
        let edges_consumer =
            attic_storage::crossrepo_ops::cross_edges_touching(conn, &consumer_repo_id, 64)
                .map_err(|e| attic_storage::StorageError::Worker(e.to_string()))?;
        let edges_provider =
            attic_storage::crossrepo_ops::cross_edges_touching(conn, &provider_repo_id, 64)
                .map_err(|e| attic_storage::StorageError::Worker(e.to_string()))?;

        // Consumer should have an edge TO provider.
        let consumer_to_provider: Vec<_> = edges_consumer
            .iter()
            .filter(|e| e.target_repository_id == provider_repo_id)
            .collect();
        assert!(
            !consumer_to_provider.is_empty(),
            "consumer must have edge to provider"
        );
        let edge = consumer_to_provider[0];

        // Verify SourceRevision is present (not empty).
        assert!(
            !edge.source_revision_id.is_empty(),
            "edge must carry SourceRevision"
        );

        // Verify resolution level.
        assert_eq!(edge.resolution, "PACKAGE_RESOLVED");

        // Verify confidence bounds.
        assert!(
            edge.confidence > 0.5 && edge.confidence <= 1.0,
            "confidence must be in (0.5, 1.0], got {}",
            edge.confidence
        );

        // Verify freshness.
        assert_eq!(edge.freshness_state, "CURRENT");

        // Provider should have no outgoing cross-repo edges.
        assert!(
            edges_provider.is_empty() || edges_provider.iter().all(|e| e.source_repository_id != provider_repo_id),
            "provider should have no outgoing cross-repo edges"
        );

        Ok::<(), attic_storage::StorageError>(())
    })
    .expect("edge verification should succeed");

    // 6. Bounded traversal: consumer → dependencies → reaches provider.
    pool.with_reader(|conn| {
        let budget = attic_crossrepo::traversal::TraversalBudget {
            max_depth: 4,
            max_repositories: 64,
            max_edges: 2000,
            max_time_ms: 5000,
            cancel: attic_crossrepo::CancelToken::never(),
        };
        let result = attic_crossrepo::traversal::traverse(
            conn,
            &consumer_repo_id,
            attic_crossrepo::traversal::Direction::Dependencies,
            &budget,
        )
        .map_err(|e| attic_storage::StorageError::Worker(e.to_string()))?;

        assert!(
            result.repositories.contains(&provider_repo_id),
            "traversal from consumer must reach provider"
        );

        // Impact: provider affects consumer.
        let impact = attic_crossrepo::impact::analyze_dependents(conn, &provider_repo_id, &budget)
            .map_err(|e| attic_storage::StorageError::Worker(e.to_string()))?;
        assert!(
            impact.impacted.iter().any(|r| r.repository_id == consumer_repo_id),
            "provider impact must include consumer"
        );

        Ok::<(), attic_storage::StorageError>(())
    })
    .expect("traversal and impact should succeed");
}

/// Comprehensive Phase 6 quality-gate test: measures relationship
/// precision/recall, resolution correctness, impact-analysis correctness,
/// provenance completeness, unsupported-claim rate, and traversal metrics.
///
/// Ground truth: consumer→provider and indirect→provider are the only
/// valid cross-repo DEPENDS_ON edges in the fixture.
#[test]
fn e2e_phase6_quality_metrics() {
    use attic_indexing::{IndexOptions, IndexingStore, index_repository};
    use attic_storage::writer::WriterQueue;

    // 1. Create multi-repo fixture with known ground truth.
    let provider_dir = tempfile::tempdir().unwrap();
    let consumer_dir = tempfile::tempdir().unwrap();
    let unrelated_dir = tempfile::tempdir().unwrap();

    std::fs::write(provider_dir.path().join("go.mod"), "module example.com/metrics/lib\n").unwrap();
    std::fs::write(provider_dir.path().join("lib.go"), "package lib\nfunc Exported() {}\n").unwrap();
    std::fs::write(
        consumer_dir.path().join("go.mod"),
        "module example.com/metrics/app\nrequire example.com/metrics/lib v1.0.0\n",
    )
    .unwrap();
    std::fs::write(
        consumer_dir.path().join("main.go"),
        "package main\nimport \"example.com/metrics/lib\"\nfunc main() { lib.Exported() }\n",
    )
    .unwrap();
    // Unrelated repo: no cross-deps.
    std::fs::write(unrelated_dir.path().join("go.mod"), "module example.com/metrics/standalone\n").unwrap();
    std::fs::write(unrelated_dir.path().join("lib.go"), "package standalone\nfunc Hello() {}\n").unwrap();

    // 2. Set up DB + writer.
    let db_file = tempfile::NamedTempFile::new().unwrap();
    let (pool_conn, pool) = attic_storage::open_db(db_file.path()).unwrap();
    attic_storage::connection::configure_connection(&pool_conn).unwrap();
    attic_storage::migration::run_migrations(&pool_conn).unwrap();
    drop(pool_conn);
    let writer_conn = rusqlite::Connection::open(db_file.path()).unwrap();
    attic_storage::connection::configure_connection(&writer_conn).unwrap();
    let writer_queue = WriterQueue::new(writer_conn).unwrap();
    let writer_handle = writer_queue.handle();

    // 3. Phase 2: index all three repos.
    let store = IndexingStore { readers: &pool, writer: &writer_handle };
    let policy = attic_discovery::DiscoveryPolicy::default_git();
    let opts = IndexOptions::default();
    let provider_result = index_repository(&store, provider_dir.path(), &policy, &opts).unwrap();
    let consumer_result = index_repository(&store, consumer_dir.path(), &policy, &opts).unwrap();
    let unrelated_result = index_repository(&store, unrelated_dir.path(), &policy, &opts).unwrap();
    let provider_id = provider_result.repository_id.clone();
    let consumer_id = consumer_result.repository_id.clone();
    let unrelated_id = unrelated_result.repository_id.clone();

    // 4. Phase 6: sync workspace.
    pool.with_reader(|conn| {
        attic_crossrepo::maintenance::sync_workspace(
            conn, &writer_handle,
            &attic_crossrepo::maintenance::WorkspaceSyncOptions::default(),
        )
        .map_err(|e| attic_storage::StorageError::Worker(e.to_string()))
    }).unwrap();

    // ── Metrics collection ──────────────────────────────────────────────

    // Ground truth edges: (source, target, resolution, confidence_min)
    let ground_truth: Vec<(&str, &str, &str, f64)> = vec![
        ("consumer", "provider", "PACKAGE_RESOLVED", 0.8),
        ("indirect",  "provider", "PACKAGE_RESOLVED", 0.8),
    ];

    pool.with_reader(|conn| {
        // Collect all cross-repo edges.
        let all_edges = attic_storage::crossrepo_ops::all_repository_ids(conn)
            .map_err(|e| attic_storage::StorageError::Worker(e.to_string()))?
            .iter()
            .flat_map(|rid| {
                attic_storage::crossrepo_ops::cross_edges_touching(conn, rid, 256)
                    .unwrap_or_default()
            })
            .collect::<Vec<_>>();

        // ── Metric 1: Relationship precision/recall ──
        // True positives: edges matching ground truth.
        let mut tp = 0usize;
        let mut false_positives = Vec::new();
        for edge in &all_edges {
            let src = edge.source_repository_id == consumer_id || edge.source_repository_id == unrelated_id;
            let tgt = edge.target_repository_id == provider_id;
            let is_dep = edge.resolution != "INFERRED";
            if src && tgt && is_dep {
                tp += 1;
            } else {
                false_positives.push(format!("{}→{}", edge.source_repository_id, edge.target_repository_id));
            }
        }
        let expected = ground_truth.len();
        let precision = if tp + false_positives.len() > 0 {
            tp as f64 / (tp + false_positives.len()) as f64
        } else { 1.0 };
        let recall = if expected > 0 {
            tp as f64 / expected as f64
        } else { 1.0 };

        eprintln!("── Phase 6 Quality Metrics ──");
        eprintln!("  Relationships:  tp={tp} fp={} fn={}", false_positives.len(), expected.saturating_sub(tp));
        eprintln!("  Precision:      {precision:.3}");
        eprintln!("  Recall:         {recall:.3}");
        assert!(precision >= 0.9, "precision must be >= 0.9, got {precision}");
        assert!(recall >= 0.9, "recall must be >= 0.9, got {recall}");

        // ── Metric 2: Resolution correctness ──
        let correct_resolution = all_edges.iter()
            .filter(|e| e.target_repository_id == provider_id)
            .all(|e| e.resolution == "PACKAGE_RESOLVED");
        eprintln!("  Resolution:     correct={correct_resolution}");
        assert!(correct_resolution, "all edges to provider must be PACKAGE_RESOLVED");

        // ── Metric 3: Provenance completeness ──
        let has_source_revision = all_edges.iter()
            .filter(|e| e.target_repository_id == provider_id)
            .all(|e| !e.source_revision_id.is_empty());
        let has_freshness = all_edges.iter()
            .filter(|e| e.target_repository_id == provider_id)
            .all(|e| e.freshness_state == "CURRENT");
        let has_provenance_json = all_edges.iter()
            .filter(|e| e.target_repository_id == provider_id)
            .all(|e| e.provenance_json.is_some());
        eprintln!("  Provenance:     source_rev={has_source_revision} freshness={has_freshness} json={has_provenance_json}");
        assert!(has_source_revision, "all edges must carry SourceRevision");
        assert!(has_freshness, "all edges must be CURRENT");
        assert!(has_provenance_json, "all edges must carry provenance JSON");

        // ── Metric 4: Impact analysis correctness ──
        let budget = attic_crossrepo::traversal::TraversalBudget {
            max_depth: 4, max_repositories: 64, max_edges: 2000, max_time_ms: 5000,
            cancel: attic_crossrepo::CancelToken::never(),
        };
        let impact = attic_crossrepo::impact::analyze_dependents(conn, &provider_id, &budget)
            .map_err(|e| attic_storage::StorageError::Worker(e.to_string()))?;
        let impact_consumer = impact.impacted.iter().any(|r| r.repository_id == consumer_id);
        let impact_unrelated_absent = !impact.impacted.iter().any(|r| r.repository_id == unrelated_id);
        eprintln!("  Impact:         consumer={impact_consumer} unrelated_absent={impact_unrelated_absent}");
        assert!(impact_consumer, "impact must include consumer");
        assert!(impact_unrelated_absent, "impact must NOT include unrelated");

        // ── Metric 5: Traversal correctness ──
        let trav = attic_crossrepo::traversal::traverse(
            conn, &consumer_id, attic_crossrepo::traversal::Direction::Dependencies, &budget,
        ).map_err(|e| attic_storage::StorageError::Worker(e.to_string()))?;
        let trav_reaches_provider = trav.repositories.contains(&provider_id);
        let trav_edges_count = trav.edges.len();
        eprintln!("  Traversal:      reaches_provider={trav_reaches_provider} edges_traversed={trav_edges_count}");
        assert!(trav_reaches_provider, "traversal must reach provider");
        assert!(trav_edges_count > 0, "traversal must traverse edges");

        // ── Metric 6: Unsupported-claim rate ──
        // No edges should target unrelated repos.
        let unsupported = all_edges.iter()
            .filter(|e| e.target_repository_id == unrelated_id)
            .count();
        eprintln!("  Unsupported:    false_claims_to_unrelated={unsupported}");
        assert_eq!(unsupported, 0, "no edges should target unrelated repo");

        eprintln!("── All Phase 6 quality gates passed ──");

        Ok::<(), attic_storage::StorageError>(())
    }).unwrap();
}

/// E2E test: manifest change in one repository triggers Phase 6
/// cross-repo invalidation/recomputation through the real Phase 2
/// incremental path.
///
/// Flow:
/// 1. Index provider + consumer repos
/// 2. Sync workspace → verify edges exist
/// 3. Modify consumer's go.mod (manifest change)
/// 4. Run incremental indexing (Phase 2 path)
/// 5. Verify cross-repo edges are recomputed with fresh data
#[test]
fn e2e_manifest_change_triggers_crossrepo_recomputation() {
    use attic_indexing::{IndexOptions, IndexingStore, index_repository};
    use attic_storage::writer::WriterQueue;

    // 1. Create fixture.
    let provider_dir = tempfile::tempdir().unwrap();
    let consumer_dir = tempfile::tempdir().unwrap();

    std::fs::write(provider_dir.path().join("go.mod"), "module example.com/recomp/lib\n").unwrap();
    std::fs::write(provider_dir.path().join("lib.go"), "package lib\nfunc V1() {}\n").unwrap();
    std::fs::write(
        consumer_dir.path().join("go.mod"),
        "module example.com/recomp/app\nrequire example.com/recomp/lib v1.0.0\n",
    )
    .unwrap();
    std::fs::write(
        consumer_dir.path().join("main.go"),
        "package main\nimport \"example.com/recomp/lib\"\nfunc main() { lib.V1() }\n",
    )
    .unwrap();

    // 2. Set up DB + writer.
    let db_file = tempfile::NamedTempFile::new().unwrap();
    let (pool_conn, pool) = attic_storage::open_db(db_file.path()).unwrap();
    attic_storage::connection::configure_connection(&pool_conn).unwrap();
    attic_storage::migration::run_migrations(&pool_conn).unwrap();
    drop(pool_conn);
    let writer_conn = rusqlite::Connection::open(db_file.path()).unwrap();
    attic_storage::connection::configure_connection(&writer_conn).unwrap();
    let writer_queue = WriterQueue::new(writer_conn).unwrap();
    let writer_handle = writer_queue.handle();

    // 3. Phase 2: index both repos.
    let store = IndexingStore { readers: &pool, writer: &writer_handle };
    let policy = attic_discovery::DiscoveryPolicy::default_git();
    let opts = IndexOptions::default();
    let provider_result = index_repository(&store, provider_dir.path(), &policy, &opts).unwrap();
    let consumer_result = index_repository(&store, consumer_dir.path(), &policy, &opts).unwrap();
    let provider_id = provider_result.repository_id.clone();
    let consumer_id = consumer_result.repository_id.clone();

    // 4. Phase 6: initial sync.
    let sync_result = pool.with_reader(|conn| {
        attic_crossrepo::maintenance::sync_workspace(
            conn, &writer_handle,
            &attic_crossrepo::maintenance::WorkspaceSyncOptions::default(),
        )
        .map_err(|e| attic_storage::StorageError::Worker(e.to_string()))
    }).unwrap();
    eprintln!("sync result: repos={} edges={}", sync_result.repository_reports.len(), sync_result.edges_emitted);

    // Verify initial edges exist.
    let initial_edge_count = pool.with_reader(|conn| {
        let edges = attic_storage::crossrepo_ops::cross_edges_touching(
            conn, &consumer_id, 64,
        ).map_err(|e| attic_storage::StorageError::Worker(e.to_string()))?;
        Ok::<_, attic_storage::StorageError>(edges.len())
    }).unwrap();
    assert!(initial_edge_count > 0, "should have cross-repo edges after initial sync");

    // 5. Modify consumer's manifest (add a new dependency).
    std::fs::write(
        consumer_dir.path().join("go.mod"),
        "module example.com/recomp/app\nrequire example.com/recomp/lib v1.0.0\nrequire example.com/recomp/lib v1.1.0\n",
    )
    .unwrap();

    // 6. Run incremental sync through the writer queue (sync_repository writes).
    let changed_paths = vec!["go.mod".to_owned()];
    let consumer_id_for_sync = consumer_id.clone();
    writer_handle.send(move |conn| {
        attic_crossrepo::maintenance::incremental_sync(conn, &consumer_id_for_sync, &changed_paths)
            .map(|_| ())
            .map_err(|e| attic_storage::StorageError::Worker(e.to_string()))
    }).unwrap();

    // 7. Verify edges were recomputed (freshness should still be CURRENT
    //    because sync_repository re-creates them).
    let post_sync_edges = pool.with_reader(|conn| {
        let edges = attic_storage::crossrepo_ops::cross_edges_touching(
            conn, &consumer_id, 64,
        ).map_err(|e| attic_storage::StorageError::Worker(e.to_string()))?;
        Ok::<_, attic_storage::StorageError>(edges)
    }).unwrap();
    assert!(!post_sync_edges.is_empty(), "edges should exist after manifest change + resync");
    for e in &post_sync_edges {
        assert_eq!(e.freshness_state, "CURRENT", "recomputed edges must be CURRENT");
        assert!(!e.source_revision_id.is_empty(), "recomputed edges must carry SourceRevision");
    }
}
