//! Lexical/structural indexing throughput benchmark (full cold, warm
//! re-index, and scoped incremental), with per-stage timings.
//!
//! Env-gated (`ATTIC_BENCH_INDEX=1`) and `#[ignore]`d. Run optimized so the
//! tree-sitter C grammars and the analyzers are measured as they ship:
//!
//! ```text
//! $env:ATTIC_BENCH_INDEX='1'
//! cargo test --release -p attic-indexing --test index_throughput_bench -- --ignored --nocapture
//! ```
//!
//! Corpus: `ATTIC_BENCH_ROOT` (an existing directory, indexed in place and
//! never modified) or — by default — a synthetic, skew-sized corpus built in
//! a temp dir from this workspace's own sources replicated
//! `ATTIC_BENCH_REPLICAS` (default 6) times plus a few multi-megabyte JSON
//! documents, which is what makes static work partitioning visible.
//! `ATTIC_BENCH_THREADS` pins `IndexOptions::analysis_threads` (default 0 =
//! auto). The incremental phase edits `ATTIC_BENCH_INCR_FILES` (default 300)
//! files of the synthetic corpus and republishes them with `index_changes`.

use std::path::{Path, PathBuf};
use std::time::Instant;

use attic_discovery::DiscoveryPolicy;
use attic_indexing::{IndexOptions, IndexingStore, ScopedChanges, index_changes, index_repository};
use attic_storage::{WriterConfig, WriterQueue, open_db_with_pragmas};

fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(default)
}

fn collect_sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            if p.file_name()
                .is_some_and(|n| n != "target" && n != "fixtures")
            {
                collect_sources(&p, out);
            }
        } else if p
            .extension()
            .is_some_and(|x| x == "rs" || x == "toml" || x == "md")
        {
            out.push(p);
        }
    }
}

/// Deterministic JSON document of roughly `bytes` bytes (many small objects,
/// like the environment exports that dominate real corpora).
fn big_json(seed: usize, bytes: usize) -> String {
    let mut s = String::from("{\"components\":[");
    let mut i = 0usize;
    while s.len() < bytes {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!(
            "{{\"id\":\"c{seed}-{i}\",\"type\":\"field\",\"label\":\"Field {i}\",\
             \"validation\":{{\"required\":{},\"max\":{}}},\"props\":{{\"x\":{},\"y\":\"v{}\"}}}}",
            i.is_multiple_of(2),
            i % 97,
            i % 13,
            i % 7
        ));
        i += 1;
    }
    s.push_str("]}");
    s
}

/// Returns the list of repo-relative source paths (forward slashes).
fn build_synthetic(root: &Path, replicas: usize) -> Vec<String> {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join("..");
    let mut sources = Vec::new();
    collect_sources(&workspace.join("crates"), &mut sources);
    collect_sources(&workspace.join("docs"), &mut sources);
    sources.sort();
    let mut rels = Vec::new();
    for r in 0..replicas {
        for src in &sources {
            let rel_src = src.strip_prefix(&workspace).unwrap();
            let rel = Path::new(&format!("replica{r}")).join(rel_src);
            let dst = root.join(&rel);
            std::fs::create_dir_all(dst.parent().unwrap()).unwrap();
            std::fs::copy(src, &dst).unwrap();
            rels.push(rel.to_string_lossy().replace('\\', "/"));
        }
    }
    std::fs::create_dir_all(root.join("exports")).unwrap();
    for j in 0..4 {
        let rel = format!("exports/env{j}.json");
        std::fs::write(root.join(&rel), big_json(j, 3 * 1024 * 1024)).unwrap();
    }
    rels
}

#[test]
#[ignore = "throughput benchmark; set ATTIC_BENCH_INDEX=1 and run with --ignored"]
fn index_throughput_full_warm_incremental() {
    if std::env::var("ATTIC_BENCH_INDEX").ok().as_deref() != Some("1") {
        eprintln!("ATTIC_BENCH_INDEX!=1; skipping");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let (root, rels): (PathBuf, Vec<String>) = match std::env::var("ATTIC_BENCH_ROOT") {
        Ok(r) => (PathBuf::from(r), Vec::new()),
        Err(_) => {
            let root = tmp.path().join("corpus");
            let rels = build_synthetic(&root, env_usize("ATTIC_BENCH_REPLICAS", 6));
            (root, rels)
        }
    };

    let db_path = tmp.path().join("bench.db");
    let (conn, pool) = open_db_with_pragmas(&db_path, -65536, 256 * 1024 * 1024).unwrap();
    attic_storage::run_migrations(&conn).unwrap();
    let queue = WriterQueue::new_with_config(
        conn,
        WriterConfig {
            queue_capacity: 8192,
            batch_size: 512,
            flush_interval: std::time::Duration::from_millis(25),
            max_io_ops_per_sec: u32::MAX,
        },
    )
    .unwrap();
    let writer = queue.handle();
    let store = IndexingStore {
        readers: &pool,
        writer: &writer,
    };
    let policy = if root.join(".git").exists() {
        DiscoveryPolicy::default_git()
    } else {
        DiscoveryPolicy::default_non_git()
    };
    let opts = IndexOptions {
        analysis_threads: env_usize("ATTIC_BENCH_THREADS", 0),
        ..IndexOptions::default()
    };

    for label in ["cold", "warm"] {
        let t0 = Instant::now();
        let r = index_repository(&store, &root, &policy, &opts).expect("index succeeds");
        let t = &r.stage_timings;
        eprintln!(
            "INDEX_BENCH phase={label} files={} units={} threads={} wall_ms={} \
             discovery={} collect={} analysis={} resolve={} publish={} total={}",
            r.files_indexed,
            r.units_inserted,
            t.analysis_threads_used,
            t0.elapsed().as_millis(),
            t.discovery_ms,
            t.collect_ms,
            t.analysis_ms,
            t.resolve_ms,
            t.publish_ms,
            t.total_ms,
        );
    }

    if rels.is_empty() {
        return;
    }
    let k = env_usize("ATTIC_BENCH_INCR_FILES", 300).min(rels.len());
    let upserts: Vec<String> = rels
        .iter()
        .step_by(rels.len() / k)
        .take(k)
        .cloned()
        .collect();
    for rel in &upserts {
        let p = root.join(rel);
        let mut text = std::fs::read_to_string(&p).unwrap();
        text.push_str("\n// bench edit\n");
        std::fs::write(&p, text).unwrap();
    }
    let t0 = Instant::now();
    let r = index_changes(
        &store,
        &root,
        &policy,
        &opts,
        &ScopedChanges {
            upserts: upserts.clone(),
            ..Default::default()
        },
    )
    .expect("incremental succeeds");
    eprintln!(
        "INDEX_BENCH phase=incremental files={} units={} wall_ms={}",
        r.files_published,
        r.units_inserted,
        t0.elapsed().as_millis()
    );
}
