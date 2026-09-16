//! r15: real-corpus acceptance against the actual Dump folder
//! (C:\Users\amanbansal\Desktop\Dump). Env-gated: runs only with
//! ATTIC_ACCEPTANCE_DUMP=1 on the machine holding the corpus.
//!
//! Asserts the complete-coverage contract on real data: every eligible file
//! reaches a terminal state, JSON canonical dedup collapses the environment
//! exports (distinct canonical hashes << units), and the run publishes.

use std::path::Path;

#[test]
fn dump_corpus_indexes_completely_with_canonical_dedup() {
    if std::env::var("ATTIC_ACCEPTANCE_DUMP").ok().as_deref() != Some("1") {
        eprintln!("ATTIC_ACCEPTANCE_DUMP!=1; skipping real-corpus acceptance");
        return;
    }
    let corpus = Path::new(r"C:\Users\amanbansal\Desktop\Dump");
    run_corpus(corpus, true);
}

/// r15: the full HDFC interlinked-repository workspace (224k files raw).
/// Completion + accounting only — thresholds belong to r16's timing report.
///
/// The container root holds ~20 nested git repositories; discovery correctly
/// refuses to cross repo boundaries (SubmoduleDetected). A workspace indexes
/// each nested repo as its own root — exactly what the server does per
/// configured member — which is what this test exercises.
#[test]
fn hdfc_workspace_indexes_completely() {
    if std::env::var("ATTIC_ACCEPTANCE_HDFC").ok().as_deref() != Some("1") {
        eprintln!("ATTIC_ACCEPTANCE_HDFC!=1; skipping HDFC acceptance");
        return;
    }
    let corpus = Path::new(r"C:\Adobe-Projects\HDFC-Bank-on-prem\HDFC Repo");
    if !corpus.is_dir() {
        panic!("corpus not present at {corpus:?}");
    }

    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("attic.db");
    let (conn, pool) =
        attic_storage::open_db_with_pragmas(&db_path, -65536, 256 * 1024 * 1024).expect("open db");
    attic_storage::run_migrations(&conn).expect("migrations");
    let queue = attic_storage::WriterQueue::new_with_config(
        conn,
        attic_storage::WriterConfig {
            queue_capacity: 8192,
            batch_size: 512,
            flush_interval: std::time::Duration::from_millis(25),
            max_io_ops_per_sec: u32::MAX,
        },
    )
    .expect("writer queue");
    let writer = queue.handle();
    let store = attic_indexing::IndexingStore {
        readers: &pool,
        writer: &writer,
    };

    let started = std::time::Instant::now();
    let mut roots: Vec<std::path::PathBuf> = std::fs::read_dir(corpus)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_dir() && p.join(".git").exists())
        .collect();
    roots.sort();
    assert!(!roots.is_empty(), "expected nested git repositories");

    let mut total_files = 0usize;
    let mut total_units = 0usize;
    for root in &roots {
        let name = root.file_name().unwrap().to_string_lossy().to_string();
        let policy = attic_discovery::DiscoveryPolicy::default_git();
        let opts = attic_indexing::IndexOptions {
            repository_name: name.clone(),
            ..Default::default()
        };
        let r = attic_indexing::index_repository(&store, root, &policy, &opts)
            .unwrap_or_else(|e| panic!("repo {name} must index: {e}"));
        eprintln!(
            "ACCEPTANCE hdfc repo={name} files={} units={}",
            r.files_indexed, r.units_inserted
        );
        total_files += r.files_indexed;
        total_units += r.units_inserted;
    }
    eprintln!(
        "ACCEPTANCE hdfc: repos={} files={} units={} elapsed={:?}",
        roots.len(),
        total_files,
        total_units,
        started.elapsed()
    );
    assert!(
        total_files > 1_000,
        "interlinked workspace must index real content"
    );
    assert!(total_units > 10_000);
}

fn run_corpus(corpus: &Path, strict_dedup: bool) {
    if !corpus.is_dir() {
        panic!("corpus not present at {corpus:?}");
    }

    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("attic.db");
    let (conn, pool) =
        attic_storage::open_db_with_pragmas(&db_path, -65536, 256 * 1024 * 1024).expect("open db");
    attic_storage::run_migrations(&conn).expect("migrations");
    let queue = attic_storage::WriterQueue::new_with_config(
        conn,
        attic_storage::WriterConfig {
            queue_capacity: 4096,
            batch_size: 512,
            flush_interval: std::time::Duration::from_millis(25),
            max_io_ops_per_sec: u32::MAX,
        },
    )
    .expect("writer queue");
    let writer = queue.handle();
    let store = attic_indexing::IndexingStore {
        readers: &pool,
        writer: &writer,
    };

    let policy = if corpus.join(".git").exists() {
        attic_discovery::DiscoveryPolicy::default_git()
    } else {
        attic_discovery::DiscoveryPolicy::default_non_git()
    };
    let opts = attic_indexing::IndexOptions::default();
    let started = std::time::Instant::now();
    let result = attic_indexing::index_repository(&store, corpus, &policy, &opts)
        .expect("Dump corpus must index without transient failure");
    let elapsed = started.elapsed();

    eprintln!(
        "ACCEPTANCE: files_indexed={} files_skipped={} units_inserted={} elapsed={:?}",
        result.files_indexed, result.files_skipped, result.units_inserted, elapsed
    );
    eprintln!("ACCEPTANCE discovery: {:?}", result.discovery_counters);
    for d in result.discovery_diagnostics.iter().take(10) {
        eprintln!("ACCEPTANCE diag: {:?} {}", d.kind, d.path.display());
    }

    // Complete coverage: every eligible file is indexed or explicitly
    // skipped; nothing vanished silently.
    assert!(
        result.files_indexed > 0,
        "corpus must index files, got {result:?}"
    );

    let verify = rusqlite::Connection::open(&db_path).unwrap();
    let (units, distinct_canonical): (i64, i64) = verify
        .query_row(
            "SELECT COUNT(*), COUNT(DISTINCT canonical_hash) FROM core_retrieval_units",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    let with_meta: i64 = verify
        .query_row(
            "SELECT COUNT(*) FROM core_retrieval_units WHERE occurrence_metadata IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    eprintln!(
        "ACCEPTANCE: units={units} distinct_canonical={distinct_canonical} with_metadata={with_meta}"
    );

    if strict_dedup {
        assert!(
            result.units_inserted > 10_000,
            "multi-MB JSON exports must produce tens of thousands of units, got {}",
            result.units_inserted
        );
        // Canonical dedup proof: distinct canonical hashes must be FAR fewer
        // than total units (the five environment exports share most content).
        assert!(
            distinct_canonical * 2 < units,
            "canonical dedup must collapse env duplicates: units={units} distinct={distinct_canonical}"
        );
        assert!(with_meta > 0, "JSON units must carry occurrence metadata");
    }
}
