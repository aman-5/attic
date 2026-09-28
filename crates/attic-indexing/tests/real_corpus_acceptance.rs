//! r15/r17: real-corpus acceptance against the actual Dump folder
//! (C:\Users\amanbansal\Desktop\Dump). Env-gated: runs only with
//! ATTIC_ACCEPTANCE_DUMP=1 on the machine holding the corpus.
//!
//! Asserts the complete-coverage contract on real data: every eligible file
//! reaches a terminal state, JSON canonical dedup collapses the environment
//! exports (distinct canonical hashes << units), and the run publishes.
//!
//! r17: thresholds tightened from loose lower bounds to EXACT frozen counts
//! (plan Phase 8 gate — "never lower the threshold, explain every
//! difference"). These numbers were captured by actually running this test
//! against the real corpus on 2026-09-27, not carried forward from an older
//! report: `files_seen=18 files_indexed=15 files_skipped=3 units=45110
//! distinct_canonical=10596`. `files_skipped=3` accounts for exactly the two
//! DOCX files and the one PDF that discovery classifies with an explicit
//! unsupported verdict (see `attic_discovery`'s
//! `pdf_and_docx_get_explicit_unsupported_verdict` unit test for that
//! classification proof) — nothing else is silently dropped. If a future
//! run of this test fails on these exact numbers because the Dump folder's
//! contents legitimately changed, update the constants below to the new
//! measured values and say so in the commit message; do not loosen them
//! back into inequalities.

use std::path::Path;

/// Exact counts measured against the real corpus (see module docs).
const DUMP_FILES_SEEN: u64 = 18;
const DUMP_FILES_INDEXED: usize = 15;
const DUMP_FILES_SKIPPED: usize = 3; // 2 DOCX + 1 PDF, explicitly unsupported
const DUMP_UNITS_INSERTED: usize = 45_110;
const DUMP_DISTINCT_CANONICAL: i64 = 10_596;

#[test]
fn dump_corpus_indexes_completely_with_canonical_dedup() {
    if std::env::var("ATTIC_ACCEPTANCE_DUMP").ok().as_deref() != Some("1") {
        eprintln!("ATTIC_ACCEPTANCE_DUMP!=1; skipping real-corpus acceptance");
        return;
    }
    let corpus = Path::new(r"C:\Users\amanbansal\Desktop\Dump");
    run_corpus(corpus, true);
}

/// r15/r17: the full HDFC interlinked-repository workspace.
///
/// The container root holds 20 nested git repositories; discovery correctly
/// refuses to cross repo boundaries (SubmoduleDetected). A workspace indexes
/// each nested repo as its own root — exactly what the server does per
/// configured member — which is what this test exercises.
///
/// r17: exact frozen accounting (plan Phase 8 gate), captured by actually
/// running this test on 2026-09-27: `repos=20 files=7382 units=71031`.
///
/// 2026-09-28: re-measured at `repos=20 files=12161 units=95886` after
/// discovery began indexing FileVault `.content.xml` dot-files (4,779 AEM
/// JCR content files that were previously skipped as hidden) and the AEM
/// plugin added structure to them. Every repository's unit count stayed
/// equal or grew. Note: on this machine the per-repo terminal-state gate
/// already fails for `impact-analyser` on the pre-change commit (1409da65:
/// 96 terminal of 105 seen), independent of this change.
///
/// Every
/// intended repository must appear with at least one indexed file — a repo
/// silently contributing zero files is exactly the "intended repository
/// disappears silently" failure the gate forbids. If the HDFC workspace
/// legitimately changes (a repo added/removed, content edited), update
/// these constants to the new measured values rather than loosening them
/// back into inequalities.
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
    const HDFC_REPO_COUNT: usize = 20;
    const HDFC_TOTAL_FILES: usize = 12_161;
    const HDFC_TOTAL_UNITS: usize = 95_886;

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
    let mut per_repo: Vec<(String, usize, usize)> = Vec::with_capacity(roots.len());
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
        // Gate: no discovered file in this repo may be left in a retryable
        // (non-terminal) state — every eligible file is either indexed or
        // explicitly, permanently skipped.
        assert_eq!(
            r.files_indexed as u64 + r.files_skipped as u64,
            r.discovery_counters.files_seen,
            "repo {name}: every discovered file must reach a terminal state"
        );
        // Gate: no intended repository may silently contribute zero files.
        assert!(
            r.files_indexed > 0,
            "repo {name} contributed zero indexed files — it must not silently disappear"
        );
        per_repo.push((name, r.files_indexed, r.units_inserted));
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

    // r17: exact frozen accounting (plan Phase 8 gate) — not a lower bound.
    assert_eq!(
        roots.len(),
        HDFC_REPO_COUNT,
        "nested repository count drifted from the frozen baseline: found {:?}",
        per_repo.iter().map(|(n, _, _)| n).collect::<Vec<_>>()
    );
    assert_eq!(
        total_files, HDFC_TOTAL_FILES,
        "total indexed file count drifted from the frozen baseline (per-repo: {per_repo:?})"
    );
    assert_eq!(
        total_units, HDFC_TOTAL_UNITS,
        "total unit count drifted from the frozen baseline"
    );
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
        // r17: exact frozen accounting (plan Phase 8 gate) — not a lower
        // bound. `files_seen` is the raw discovery count before eligibility
        // filtering; `files_indexed + files_skipped` must equal it exactly,
        // or some file vanished between discovery and indexing without a
        // terminal state.
        assert_eq!(
            result.discovery_counters.files_seen, DUMP_FILES_SEEN,
            "raw discovered file count drifted from the frozen baseline — \
             if the Dump folder's contents legitimately changed, update \
             DUMP_FILES_SEEN and the other constants together, don't loosen them"
        );
        assert_eq!(
            result.files_indexed, DUMP_FILES_INDEXED,
            "indexed file count drifted from the frozen baseline"
        );
        assert_eq!(
            result.files_skipped, DUMP_FILES_SKIPPED,
            "skipped file count drifted — expected exactly the 2 DOCX + 1 PDF unsupported files"
        );
        assert_eq!(
            result.files_indexed as u64 + result.files_skipped as u64,
            result.discovery_counters.files_seen,
            "every discovered file must reach exactly one terminal state (indexed or skipped) — none may vanish silently"
        );
        assert_eq!(
            result.units_inserted, DUMP_UNITS_INSERTED,
            "retrieval unit count drifted from the frozen baseline"
        );
        assert_eq!(
            distinct_canonical, DUMP_DISTINCT_CANONICAL,
            "distinct canonical body count drifted from the frozen baseline"
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
