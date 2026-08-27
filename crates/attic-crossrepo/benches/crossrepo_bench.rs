//! Cross-repository intelligence product benchmark.
//!
//! Measures resolver, traversal, and impact analysis performance on
//! synthetic multi-repo workspaces of increasing size.
//!
//! Run: cargo bench --package attic-crossrepo

use std::collections::HashMap;

use attic_crossrepo::resolver::{self, RepoCatalogData};
use attic_crossrepo::traversal::{self, Direction, TraversalBudget};
use attic_crossrepo::impact;
use attic_crossrepo::{CancelToken, DeclarationKind, DependencyDeclaration, Ecosystem, ProvidedIdentity};

fn make_provider(id: usize, provides_name: &str) -> RepoCatalogData {
    RepoCatalogData {
        repository_id: format!("provider-{id}"),
        root_path: format!("/ws/provider-{id}"),
        source_revision_id: format!("rev-{id}"),
        provides: vec![ProvidedIdentity {
            ecosystem: Ecosystem::Go,
            name: provides_name.to_owned(),
        }],
        declarations: vec![],
        primary_anchor_occurrence: None,
        go_module_prefix: Some(format!("example.com/provider-{id}")),
    }
}

fn make_consumer(id: usize, deps: Vec<&str>) -> RepoCatalogData {
    let declarations: Vec<DependencyDeclaration> = deps
        .into_iter()
        .map(|name| DependencyDeclaration {
            path: "go.mod".to_owned(),
            ecosystem: Ecosystem::Go,
            name: name.to_owned(),
            version_req: None,
            kind: DeclarationKind::External,
            local_hint: None,
        })
        .collect();
    RepoCatalogData {
        repository_id: format!("consumer-{id}"),
        root_path: format!("/ws/consumer-{id}"),
        source_revision_id: format!("rev-{id}"),
        provides: vec![],
        declarations,
        primary_anchor_occurrence: None,
        go_module_prefix: Some(format!("example.com/consumer-{id}")),
    }
}

fn bench_resolver_10_providers(c: &mut criterion::Criterion) {
    let mut repos: Vec<RepoCatalogData> = (0..10)
        .map(|i| make_provider(i, &format!("example.com/lib{i}")))
        .collect();
    repos.push(make_consumer(0, vec![
        "example.com/lib0",
        "example.com/lib1",
        "example.com/lib2",
        "example.com/lib3",
        "example.com/lib4",
    ]));
    c.bench_function("resolver_10_providers", |b| {
        b.iter(|| resolver::resolve_workspace(&repos, &HashMap::new()))
    });
}

fn bench_resolver_100_providers(c: &mut criterion::Criterion) {
    let mut repos: Vec<RepoCatalogData> = (0..100)
        .map(|i| make_provider(i, &format!("example.com/lib{i}")))
        .collect();
    repos.push(make_consumer(0, vec![
        "example.com/lib0",
        "example.com/lib50",
        "example.com/lib99",
    ]));
    c.bench_function("resolver_100_providers", |b| {
        b.iter(|| resolver::resolve_workspace(&repos, &HashMap::new()))
    });
}

fn bench_resolver_1000_providers(c: &mut criterion::Criterion) {
    let mut repos: Vec<RepoCatalogData> = (0..1000)
        .map(|i| make_provider(i, &format!("example.com/lib{i}")))
        .collect();
    repos.push(make_consumer(0, vec![
        "example.com/lib0",
        "example.com/lib500",
        "example.com/lib999",
    ]));
    c.bench_function("resolver_1000_providers", |b| {
        b.iter(|| resolver::resolve_workspace(&repos, &HashMap::new()))
    });
}

fn bench_traversal_linear_chain(c: &mut criterion::Criterion) {
    use attic_storage::connection::configure_connection;
    use attic_storage::migration::run_migrations;

    let conn = rusqlite::Connection::open_in_memory().unwrap();
    configure_connection(&conn).unwrap();
    run_migrations(&conn).unwrap();

    let mut rev_ids = Vec::new();
    for i in 0..10 {
        let rid: attic_core::RepositoryId = uuid::Uuid::new_v5(
            &uuid::Uuid::NAMESPACE_DNS,
            format!("bench-r{i}").as_bytes(),
        )
        .to_string()
        .parse()
        .unwrap();
        attic_storage::repository::repository::upsert_repository(
            &conn,
            &rid,
            &format!("/ws/{i}"),
            &format!("bench-r{i}"),
        )
        .unwrap();
        let srid = attic_core::SourceRevisionId::new_v4();
        attic_storage::repository::source_revision::insert_source_revision(
            &conn,
            &srid,
            &rid,
            "sha",
            "2024-01-01",
            attic_core::SourceType::Git,
        )
        .unwrap();
        rev_ids.push(srid.to_string_repr().to_string());
    }
    for i in 0..9 {
        let src_id = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_DNS, format!("bench-r{i}").as_bytes()).to_string();
        let tgt_id = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_DNS, format!("bench-r{}", i + 1).as_bytes()).to_string();
        attic_storage::crossrepo_ops::insert_xrepo_edge(
            &conn,
            &src_id,
            &format!("occ-{i}"),
            &tgt_id,
            &format!("occ-{}", i + 1),
            "PACKAGE_RESOLVED",
            0.9,
            "GO_MODULE",
            "{}",
            &rev_ids[i],
        )
        .unwrap();
    }

    let seed_id = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_DNS, "bench-r0".as_bytes()).to_string();
    let budget = TraversalBudget {
        max_depth: 10,
        max_repositories: 64,
        max_edges: 2000,
        max_time_ms: 5000,
        cancel: CancelToken::never(),
    };
    c.bench_function("traversal_linear_chain_10", |b| {
        b.iter(|| traversal::traverse(&conn, &seed_id, Direction::Dependencies, &budget))
    });
}

fn bench_impact_linear_chain(c: &mut criterion::Criterion) {
    use attic_storage::connection::configure_connection;
    use attic_storage::migration::run_migrations;

    let conn = rusqlite::Connection::open_in_memory().unwrap();
    configure_connection(&conn).unwrap();
    run_migrations(&conn).unwrap();

    let mut rev_ids = Vec::new();
    for i in 0..10 {
        let rid: attic_core::RepositoryId = uuid::Uuid::new_v5(
            &uuid::Uuid::NAMESPACE_DNS,
            format!("imp-r{i}").as_bytes(),
        )
        .to_string()
        .parse()
        .unwrap();
        attic_storage::repository::repository::upsert_repository(
            &conn,
            &rid,
            &format!("/ws/{i}"),
            &format!("imp-r{i}"),
        )
        .unwrap();
        let srid = attic_core::SourceRevisionId::new_v4();
        attic_storage::repository::source_revision::insert_source_revision(
            &conn,
            &srid,
            &rid,
            "sha",
            "2024-01-01",
            attic_core::SourceType::Git,
        )
        .unwrap();
        rev_ids.push(srid.to_string_repr().to_string());
    }
    for i in 0..9 {
        let src_id = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_DNS, format!("imp-r{i}").as_bytes()).to_string();
        let tgt_id = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_DNS, format!("imp-r{}", i + 1).as_bytes()).to_string();
        attic_storage::crossrepo_ops::insert_xrepo_edge(
            &conn,
            &src_id,
            &format!("occ-{i}"),
            &tgt_id,
            &format!("occ-{}", i + 1),
            "PACKAGE_RESOLVED",
            0.9,
            "GO_MODULE",
            "{}",
            &rev_ids[i],
        )
        .unwrap();
    }

    let target_id = uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_DNS, "imp-r5".as_bytes()).to_string();
    let budget = TraversalBudget {
        max_depth: 8,
        max_repositories: 64,
        max_edges: 2000,
        max_time_ms: 5000,
        cancel: CancelToken::never(),
    };
    c.bench_function("impact_analyze_dependents_10", |b| {
        b.iter(|| impact::analyze_dependents(&conn, &target_id, &budget))
    });
}

/// Integrated benchmark: full retrieval pipeline with cross-repo evidence.
///
/// Sets up a multi-repo workspace with real manifests, indexes through
/// Phase 2, runs Phase 6 sync, then benchmarks `RetrievalService::answer()`
/// which exercises `CrossRepoGenerator` → Evidence Manager → response.
fn bench_integrated_retrieval_pipeline(c: &mut criterion::Criterion) {
    use attic_indexing::{IndexOptions, IndexingStore, index_repository};
    use attic_retrieval::pipeline::{AnswerRequest, RetrievalService};
    use attic_retrieval::AnswerMode;

    // 1. Create multi-repo fixture.
    let provider_dir = tempfile::tempdir().unwrap();
    let consumer_dir = tempfile::tempdir().unwrap();
    std::fs::write(provider_dir.path().join("go.mod"), "module example.com/bench/lib\n").unwrap();
    std::fs::write(provider_dir.path().join("lib.go"), "package lib\nfunc Exported() {}\n").unwrap();
    std::fs::write(
        consumer_dir.path().join("go.mod"),
        "module example.com/bench/app\nrequire example.com/bench/lib v1.0.0\n",
    )
    .unwrap();
    std::fs::write(
        consumer_dir.path().join("main.go"),
        "package main\nimport \"example.com/bench/lib\"\nfunc main() { lib.Exported() }\n",
    )
    .unwrap();

    // 2. Set up DB pool + writer queue.
    let db_file = tempfile::NamedTempFile::new().unwrap();
    let (pool_conn, pool) = attic_storage::open_db(db_file.path()).unwrap();
    attic_storage::connection::configure_connection(&pool_conn).unwrap();
    attic_storage::migration::run_migrations(&pool_conn).unwrap();
    drop(pool_conn);
    let writer_conn = rusqlite::Connection::open(db_file.path()).unwrap();
    attic_storage::connection::configure_connection(&writer_conn).unwrap();
    let writer_queue = attic_storage::writer::WriterQueue::new(writer_conn).unwrap();
    let writer_handle = writer_queue.handle();

    // 3. Phase 2: index both repos.
    let store = IndexingStore {
        readers: &pool,
        writer: &writer_handle,
    };
    let policy = attic_discovery::DiscoveryPolicy::default_git();
    let opts = IndexOptions::default();
    index_repository(&store, provider_dir.path(), &policy, &opts).unwrap();
    index_repository(&store, consumer_dir.path(), &policy, &opts).unwrap();

    // 4. Phase 6: sync workspace.
    pool.with_reader(|conn| {
        attic_crossrepo::maintenance::sync_workspace(
            conn,
            &writer_handle,
            &attic_crossrepo::maintenance::WorkspaceSyncOptions::default(),
        )
        .map_err(|e| attic_storage::StorageError::Worker(e.to_string()))
    })
    .unwrap();

    // 5. Benchmark retrieval pipeline with cross-repo query.
    let service = RetrievalService {
        readers: pool.clone(),
        writer: writer_handle.clone(),
        semantic: None,
        crossrepo_degraded: false,
    };
    let req = AnswerRequest::new("What depends on the library?", AnswerMode::Normal);
    c.bench_function("integrated_retrieval_crossrepo", |b| {
        b.iter(|| service.answer(&req).unwrap())
    });
}

criterion::criterion_group!(
    benches,
    bench_resolver_10_providers,
    bench_resolver_100_providers,
    bench_resolver_1000_providers,
    bench_traversal_linear_chain,
    bench_impact_linear_chain,
    bench_integrated_retrieval_pipeline,
);
criterion::criterion_main!(benches);
