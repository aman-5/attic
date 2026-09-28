//! TEMPORARY measurement probe — not part of the product; deleted after use.
//! Indexes PROBE_CORPUS into a temp DB (read-only on the corpus), runs the
//! real semantic selection policy and writes the unique canonical texts that
//! would be embedded to PROBE_OUT_<scenario>.jsonl.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::PathBuf;

#[test]
#[ignore = "manual probe"]
fn semantic_workload_probe() {
    let corpus = PathBuf::from(std::env::var("PROBE_CORPUS").expect("PROBE_CORPUS"));
    let out_prefix = std::env::var("PROBE_OUT").expect("PROBE_OUT");
    let tmp = tempfile::tempdir().unwrap();
    let (conn, pool) = attic_storage::open_db(tmp.path().join("attic.db")).unwrap();
    attic_storage::run_migrations(&conn).unwrap();
    let queue = attic_storage::WriterQueue::new(conn).unwrap();
    let writer = queue.handle();
    let store = attic_indexing::IndexingStore {
        readers: &pool,
        writer: &writer,
    };

    let mut roots: Vec<PathBuf> = std::fs::read_dir(&corpus)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.is_dir() && p.join(".git").exists())
        .collect();
    roots.sort();
    if roots.is_empty() || corpus.join(".git").exists() {
        roots = vec![corpus.clone()];
    }
    let started = std::time::Instant::now();
    for root in &roots {
        let policy = if root.join(".git").exists() {
            attic_discovery::DiscoveryPolicy::default_git()
        } else {
            attic_discovery::DiscoveryPolicy::default_non_git()
        };
        let opts = attic_indexing::IndexOptions {
            repository_name: root.file_name().unwrap().to_string_lossy().to_string(),
            ..Default::default()
        };
        attic_indexing::index_repository(&store, root, &policy, &opts).unwrap();
    }
    println!(
        "PROBE indexed repos={} in {:?}",
        roots.len(),
        started.elapsed()
    );

    let rows = pool
        .with_reader(|c| attic_storage::semantic_unit_rows(c, 5_000_000))
        .unwrap();
    let defaults = attic_semantic::SelectionConfig::default();
    let everything = attic_semantic::SelectionConfig {
        min_score: 0.0,
        max_units_per_repo: usize::MAX,
        max_units_total: usize::MAX,
        max_file_bytes: u64::MAX,
        ..attic_semantic::SelectionConfig::default()
    };
    for (label, cfg) in [("default", &defaults), ("everything", &everything)] {
        let (selected, duplicates, report) =
            attic_semantic::select_units(&rows, &HashMap::new(), cfg);
        let mut seen = HashSet::new();
        let mut bytes = 0u64;
        let mut f = std::fs::File::create(format!("{out_prefix}_{label}.jsonl")).unwrap();
        for su in &selected {
            let hash = su
                .row
                .canonical_hash
                .clone()
                .unwrap_or_else(|| attic_semantic::content_hash(&su.row.canonical_text));
            if seen.insert(hash) {
                bytes += su.row.canonical_text.len() as u64;
                writeln!(f, "{}", serde_json::to_string(&su.row.canonical_text).unwrap()).unwrap();
            }
        }
        let mut excluded: Vec<_> = report.excluded.iter().collect();
        excluded.sort();
        println!(
            "PROBE {label}: indexed_units={} selected={} duplicates_skipped={} unique_bodies={} unique_bytes={} excluded={:?}",
            rows.len(),
            selected.len(),
            duplicates.len(),
            seen.len(),
            bytes,
            excluded
        );
    }
}
